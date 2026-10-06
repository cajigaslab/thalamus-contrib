//! The audio half of the converter: turns analog data into a target audio
//! format. Raw formats are converted sample by sample; AAC is encoded with
//! FFmpeg (swresample -> audio FIFO -> aac encoder, framed as ADTS) and
//! decoded with FFmpeg (aac parser -> aac decoder).
//!
//! Integer and float samples are related the way audio code usually relates
//! them: an i16 of 32767 is just under 1.0, an i32 of 2^31 - 1 is just under
//! 1.0.

use std::collections::VecDeque;
use std::ops::{Range};
use std::os::raw::c_void;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use ffmpeg_sys_next::AVCodecConfig::{AV_CODEC_CONFIG_SAMPLE_FORMAT, AV_CODEC_CONFIG_SAMPLE_RATE};
use ffmpeg_sys_next::{self as ffi, AV_INPUT_BUFFER_PADDING_SIZE};

use crate::api::{AnalogData, AnalogEncoding, AnalogFormat, NodeData, ThalamusAPIThreadSafe};
use crate::frame_pool::{FramePool, FramePoolParams};
use crate::image_converter::AVERROR_EOF;

const AVERROR_EAGAIN: i32 = -ffi::EAGAIN;

fn av_error_string(ret: i32) -> String {
  let mut buf = [0i8; ffi::AV_ERROR_MAX_STRING_SIZE];
  let rc = unsafe { ffi::av_strerror(ret, buf.as_mut_ptr(), buf.len()) };
  if rc == 0 {
    unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
      .to_string_lossy()
      .to_string()
  } else {
    format!("error code {}", ret)
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioConverterParams {
  pub format: Option<AudioFormat>,
  pub bitrate: Option<i64>,
  /// The channel to start selecting input channels at. Non-negative indexes
  /// count from the first channel and select forwards; negative ones count
  /// from the end (-1 is the last channel) and select backwards. Selection
  /// takes the run of channels with the same format and sample interval as
  /// the starting channel.
  pub input_index: i32,
  pub samplerate: Option<i32>
}

impl Default for AudioConverterParams {
  fn default() -> Self {
    AudioConverterParams { format: None, bitrate: None, input_index: -1, samplerate: None }
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AudioFormat {
  Integer,
  Decimal,
  AAC,
  /// Encoded input is output in the sample format its decoder produces (as
  /// doubles when Thalamus has no matching format); raw input passes
  /// through.
  Decoded,
}


/// One converted analog message. It holds the converter's codec state while
/// it's read, which only holds up other pulls, not pushes.
pub struct AudioOutput<'a> {
  codec: MutexGuard<'a, CodecState>,
  frame: Option<*mut ffi::AVFrame>,
  time: Duration,
  encoded_count: i32
}

impl<'a> Drop for AudioOutput<'a> {
  fn drop(&mut self) {
    if let Some(mut frame) = self.frame {
      unsafe {
        ffi::av_frame_unref(frame);
        ffi::av_frame_free(&mut frame);
      }
    }
  }
}

impl<'a> NodeData for AudioOutput<'a> {
  fn time(&self) -> Duration {
    self.time
  }

  fn analog(&self) -> Option<&dyn AnalogData> {
    Some(self)
  }
}

impl<'a> AudioOutput<'a> {
  fn av_format(&self) -> ffi::AVSampleFormat {
    unsafe {
      self.frame.map(|f| {
        std::mem::transmute((*f).format)
      }).unwrap_or(ffi::AVSampleFormat::AV_SAMPLE_FMT_NONE)
    }
  }
}

fn planar_channel<'a, T>(frame: Option<*mut ffi::AVFrame>, channel: usize) -> &'a [T] {
  unsafe {
    let Some(frame) = frame else {
      return &[];
    };

    let channels = (*frame).ch_layout.nb_channels as usize;
    if channel >= channels || (*frame).nb_samples <= 0 {
      return &[];
    }
    
    let plane = *(*frame).extended_data.add(channel) as *const T;
    std::slice::from_raw_parts(plane, (*frame).nb_samples as usize)
  }
}

impl<'a> AnalogData for AudioOutput<'a> {
  fn data(&self, channel: i32) -> &[f64] {
    if self.av_format() == ffi::AVSampleFormat::AV_SAMPLE_FMT_DBLP {
      planar_channel(self.frame, channel as usize)
    } else {
      &[]
    }
  }

  fn short_data(&self, channel: i32) -> &[i16] {
    if self.av_format() == ffi::AVSampleFormat::AV_SAMPLE_FMT_S16P {
      planar_channel(self.frame, channel as usize)
    } else {
      &[]
    }
  }

  fn int_data(&self, channel: i32) -> &[i32] {
    if self.av_format() == ffi::AVSampleFormat::AV_SAMPLE_FMT_S32P {
      planar_channel(self.frame, channel as usize)
    } else {
      &[]
    }
  }

  fn is_short_data(&self) -> bool {
    self.av_format() == ffi::AVSampleFormat::AV_SAMPLE_FMT_S16P
  }

  fn is_int_data(&self) -> bool {
    self.av_format() == ffi::AVSampleFormat::AV_SAMPLE_FMT_S32P
  }

  /// AAC output reports its channels (names and sample intervals) with no
  /// samples; the audio is in the buffer.
  fn num_channels(&self) -> i32 {
    self.codec.input.as_ref().map_or(0, |input| input.names.len() as i32)
  }

  fn sample_interval(&self, _channel: i32) -> Duration {
    self.codec.output.map_or(Duration::ZERO, |output| output.sample_interval)
  }

  fn name(&self, channel: i32) -> &str {
    self.codec.input.as_ref()
      .and_then(|input| input.names.get(channel as usize))
      .map_or("", |name| name.as_str())
  }

  fn buffer(&self) -> &[u8] {
    self.codec.out_buffer.as_slice()
  }

  fn encoding(&self) -> AnalogEncoding {
    if let Some(encoder) = self.codec.encoder {
      unsafe {
        match (*encoder).codec_id {
          ffi::AVCodecID::AV_CODEC_ID_AAC => AnalogEncoding::AAC,
          other => panic!("Unsupported codec {:?}", other)
        }
      }
    } else {
      AnalogEncoding::None
    }
  }

  fn analog_format(&self, _channel: i32) -> AnalogFormat {
    match self.av_format() {
      ffi::AVSampleFormat::AV_SAMPLE_FMT_DBLP => AnalogFormat::Double,
      ffi::AVSampleFormat::AV_SAMPLE_FMT_S16P => AnalogFormat::Short,
      ffi::AVSampleFormat::AV_SAMPLE_FMT_S32P => AnalogFormat::Int,
      ffi::AVSampleFormat::AV_SAMPLE_FMT_NONE => AnalogFormat::Encoded,
      other => panic!("Unsupported format {:?}", other)
    }
  }

  fn encoded_count(&self) -> u64 {
    self.encoded_count as u64
  }
}

#[derive(Clone)]
struct InputChannels {
  range: Range<i32>,
  format: AnalogFormat,
  encoding: AnalogEncoding,
  sample_interval: Duration,
  names: Vec<String>,
}

fn is_compressed(encoding: AnalogEncoding) -> bool {
  encoding == AnalogEncoding::AAC
}

fn is_compressed_audio_format(encoding: AudioFormat) -> bool {
  encoding == AudioFormat::AAC
}

fn analog_to_audio_format(format: AnalogFormat, encoding: AnalogEncoding) -> AudioFormat {
  match format {
    AnalogFormat::Double => AudioFormat::Decimal,
    AnalogFormat::Short | AnalogFormat::Int => AudioFormat::Integer,
    AnalogFormat::Encoded => match encoding {
      AnalogEncoding::AAC => AudioFormat::AAC,
      _ => panic!("Unexpected input encoding, {:?}", encoding),
    }
    _ => panic!("Unsupported analog format {:?}", format)
  }
}

fn analog_to_sample_format(format: AnalogFormat) -> ffi::AVSampleFormat {
  match format {
    AnalogFormat::Double => ffi::AVSampleFormat::AV_SAMPLE_FMT_DBLP,
    AnalogFormat::Short => ffi::AVSampleFormat::AV_SAMPLE_FMT_S16P,
    AnalogFormat::Int => ffi::AVSampleFormat::AV_SAMPLE_FMT_S32P,
    _ => panic!("Unsupport analog format, {:?}", format)
  }
}

fn encoding_codec(encoding: AnalogEncoding) -> ffi::AVCodecID {
  match encoding {
    AnalogEncoding::AAC => ffi::AVCodecID::AV_CODEC_ID_AAC,
    _ => panic!("Unexpected format {:?}", encoding)
  }
}

fn audio_format_to_encoding(format: AudioFormat) -> Option<ffi::AVCodecID> {
  match format {
    AudioFormat::Integer => None,
    AudioFormat::Decimal => None,
    AudioFormat::AAC => Some(ffi::AVCodecID::AV_CODEC_ID_AAC),
    AudioFormat::Decoded => None,
  }
}

fn audio_format_to_sample_format(format: AudioFormat) -> ffi::AVSampleFormat {
  match format {
    AudioFormat::Integer => ffi::AVSampleFormat::AV_SAMPLE_FMT_S16P,
    AudioFormat::Decimal => ffi::AVSampleFormat::AV_SAMPLE_FMT_DBLP,
    _ => panic!("audio_format_to_sample_format {:?}", format),
  }
}

/// The planar version of a decoder's sample format, when a Thalamus analog
/// format holds it, and doubles otherwise (e.g. AAC's floats).
fn decoded_sample_format(decoded: ffi::AVSampleFormat) -> ffi::AVSampleFormat {
  match unsafe { ffi::av_get_planar_sample_fmt(decoded) } {
    planar @ (ffi::AVSampleFormat::AV_SAMPLE_FMT_S16P
      | ffi::AVSampleFormat::AV_SAMPLE_FMT_S32P
      | ffi::AVSampleFormat::AV_SAMPLE_FMT_DBLP) => planar,
    _ => ffi::AVSampleFormat::AV_SAMPLE_FMT_DBLP,
  }
}

/// The codec's supported values for `config`. Empty means it accepts anything.
pub(crate) fn get_codec_config<T>(codec: *const ffi::AVCodec, config: ffi::AVCodecConfig) -> &'static [T] {
  unsafe {
    let mut count = 0;
    let mut vals: *const std::ffi::c_void  = std::ptr::null_mut();
    let ret = ffi::avcodec_get_supported_config(std::ptr::null_mut(), codec, config,
                                       0, &mut vals, &mut count);
    assert!(ret >= 0, "avcodec_get_supported_config error: {}", av_error_string(ret));
    if vals.is_null() || count <= 0 {
      return &[];
    }
    std::slice::from_raw_parts(vals as *const T, count as usize)
  }
}

fn sample_format_for_codec(src_format: ffi::AVSampleFormat, codec: *const ffi::AVCodec) -> ffi::AVSampleFormat {
  let formats: &[ffi::AVSampleFormat] = get_codec_config(codec, AV_CODEC_CONFIG_SAMPLE_FORMAT);
  if formats.is_empty() || formats.contains(&src_format) {
    src_format
  } else {
    formats[0]
  }
}

/// The supported rate nearest `src_frequency`, so e.g. 44101 Hz from a
/// truncated sample interval picks 44100 rather than the first listed rate.
fn sample_rate_for_codec(src_frequency: i32, codec: *const ffi::AVCodec) -> i32 {
  let rates: &[std::os::raw::c_int] = get_codec_config(codec, AV_CODEC_CONFIG_SAMPLE_RATE);
  rates.iter().copied().min_by_key(|r| (r - src_frequency).abs()).unwrap_or(src_frequency)
}

const NANOS_PER_SEC: u128 = 1_000_000_000;

/// Rates sources commonly run at. Sample intervals are whole nanoseconds, so
/// e.g. 44.1 kHz arrives as 22675 or 22676 ns, which no integer rate inverts
/// exactly; matching against known rates recovers 44100.
const COMMON_SAMPLE_RATES: [i32; 14] = [
  8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 64000, 88200, 96000, 176400, 192000,
];

/// How far a sample interval may be from 1/rate and still count as that rate.
const RATE_MATCH_MARGIN_NANOS: u128 = 10;

/// Whether `interval` is within RATE_MATCH_MARGIN_NANOS of 1/`rate`, i.e.
/// |interval - 1e9/rate| < margin, checked without division as
/// |interval * rate - 1e9| < margin * rate.
fn interval_matches_rate(interval: Duration, rate: i32) -> bool {
  rate > 0 && (interval.as_nanos() * rate as u128).abs_diff(NANOS_PER_SEC) < RATE_MATCH_MARGIN_NANOS * rate as u128
}

/// The sample rate of `interval`. `preferred` (the converter's output rate)
/// and then the common rates win when the interval matches them to the
/// nanosecond, so a source already at the output rate isn't resampled.
/// Otherwise it's 1e9 / interval rounded to the nearest integer.
fn interval_to_rate(interval: Duration, preferred: Option<i32>) -> i32 {
  let nanos = interval.as_nanos();
  preferred.into_iter()
    .chain(COMMON_SAMPLE_RATES)
    .find(|rate| interval_matches_rate(interval, *rate))
    .unwrap_or_else(|| ((NANOS_PER_SEC + nanos / 2) / nanos) as i32)
}

/// The duration of `samples` samples at `rate`, rounded to the nanosecond.
/// Converting a whole count at once keeps the rounding error under 1 ns
/// instead of accumulating a rounded interval once per sample.
fn samples_to_duration(samples: u64, rate: i32) -> Duration {
  let rate = rate as u128;
  Duration::from_nanos(((samples as u128 * NANOS_PER_SEC + rate / 2) / rate) as u64)
}

/// The number of samples at `rate` in `duration`, rounded.
fn duration_to_samples(duration: Duration, rate: i32) -> u64 {
  ((duration.as_nanos() * rate as u128 + NANOS_PER_SEC / 2) / NANOS_PER_SEC) as u64
}

/// ADTS sampling_frequency_index values.
const AAC_SAMPLE_RATES: [i32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/// A 7-byte ADTS header (no CRC) for an AAC-LC frame of `payload_len` bytes.
/// The encoder outputs raw AAC frames; the header is what lets the AAC parser
/// split them and the decoder configure itself without extradata.
fn adts_header(payload_len: usize, sample_rate: i32, channels: i32) -> [u8; 7] {
  let rate_index = AAC_SAMPLE_RATES.iter().position(|r| *r == sample_rate)
    .unwrap_or_else(|| panic!("{} Hz has no ADTS sampling frequency index", sample_rate)) as u8;
  // Channel configuration 7 is 7.1 (8 channels); 7 channels would need a PCE.
  let channel_config = match channels {
    1..=6 => channels as u8,
    8 => 7,
    _ => panic!("{} channels can't be described in an ADTS header", channels),
  };
  let frame_len = payload_len + 7;
  // AAC-LC is audio object type 2, written as profile 1.
  let profile = 1u8;
  [
    0xFF,
    // Rest of the sync word, MPEG-4, layer 0, no CRC.
    0xF1,
    (profile << 6) | (rate_index << 2) | (channel_config >> 2),
    ((channel_config & 3) << 6) | ((frame_len >> 11) as u8 & 0x03),
    ((frame_len >> 3) & 0xFF) as u8,
    (((frame_len & 7) as u8) << 5) | 0x1F,
    // Buffer fullness 0x7FF (variable bitrate), one raw data block.
    0xFC,
  ]
}

/// The channels to convert out of `num_channels`: the run of channels whose
/// `kind` (format and sample interval) matches the channel at `index`, going
/// forwards from a non-negative index or backwards from a negative one (-1 is
/// the last channel). None if `index` is outside the channels.
fn select_input_channels<K: PartialEq>(index: i32, num_channels: i32, kind: impl Fn(i32) -> K) -> Option<Range<i32>> {
  let start = if index >= 0 { index } else { num_channels + index };
  if start < 0 || start >= num_channels {
    return None;
  }
  let start_kind = kind(start);
  if index >= 0 {
    let end = (start..num_channels).find(|&i| kind(i) != start_kind).unwrap_or(num_channels);
    Some(start..end)
  } else {
    let first = (0..=start).rev().find(|&i| kind(i) != start_kind).map_or(0, |i| i + 1);
    Some(first..start + 1)
  }
}

/// Whether the selected channel run of an input can be converted.
fn is_supported_input(channels: &InputChannels) -> bool {
  match channels.format {
    AnalogFormat::Double | AnalogFormat::Short | AnalogFormat::Int => true,
    AnalogFormat::Encoded => channels.encoding == AnalogEncoding::AAC,
    AnalogFormat::ULong => false,
  }
}

fn encoder_time_base(frame_interval: Duration) -> ffi::AVRational {
  // Exact: the interval is a whole number of nanoseconds.
  let mut time_base = ffi::AVRational { num: 0, den: 1 };
  unsafe {
    ffi::av_reduce(&mut time_base.num, &mut time_base.den, frame_interval.as_nanos() as i64, NANOS_PER_SEC as i64, i32::MAX as i64);
  }
  time_base
}

fn single_channel(layout: &ffi::AVChannelLayout) -> ffi::AVChannelLayout {
  unsafe {
    let channel = ffi::av_channel_layout_channel_from_index(layout, 0);
    assert!((channel as i32) >= 0, "av_channel_layout_channel_from_index returned negative");
    
    let mut mono: ffi::AVChannelLayout = std::mem::zeroed();
    let ret = ffi::av_channel_layout_from_mask(&mut mono, 1u64 << (channel as u32));
    assert!(ret >= 0, "av_channel_layout_from_mask: {}", av_error_string(ret));
    mono
  }
}

fn analog_data_ptr(analog: &dyn AnalogData, channel: i32) -> *const u8 {
  match analog.analog_format(channel) {
    AnalogFormat::Double => analog.data(channel).as_ptr() as *const u8,
    AnalogFormat::Short => analog.short_data(channel).as_ptr() as *const u8,
    AnalogFormat::Int => analog.int_data(channel).as_ptr() as *const u8,
    AnalogFormat::ULong => analog.ulong_data(channel).as_ptr() as *const u8,
    AnalogFormat::Encoded => panic!("Can't get ptr to encoded data"),
  }
}

fn analog_data_ptr_range(analog: &dyn AnalogData, channel: Range<i32>) -> impl Iterator<Item=*const u8> {
  channel.map(|i| analog_data_ptr(analog, i))
}

/// The output format, chosen from the parameters and the input's format and
/// rate.
#[derive(Debug, Clone, Copy)]
struct OutputConfig {
  sample_format: ffi::AVSampleFormat,
  sample_rate: i32,
  sample_interval: Duration,
  /// The encoder to use, if any. Only pull opens it.
  codec_id: Option<ffi::AVCodecID>,
  bitrate: i64,
}

fn choose_output(params: &AudioConverterParams, input: &InputChannels,
                 src_sample_format: ffi::AVSampleFormat, src_sample_rate: i32) -> OutputConfig {
  let src_audio_format = analog_to_audio_format(input.format, input.encoding);
  let dst_audio_format = match params.format {
    None => src_audio_format,
    Some(AudioFormat::Decoded) if !is_compressed_audio_format(src_audio_format) => src_audio_format,
    Some(format) => format,
  };
  let codec_id = audio_format_to_encoding(dst_audio_format);
  let codec = codec_id.map(|codec_id| {
    let codec = unsafe { ffi::avcodec_find_encoder(codec_id) };
    if codec.is_null() {
      panic!("no {:?} encoder in this FFmpeg build", codec_id);
    }
    codec
  });
  let sample_format = codec
    .map(|codec| sample_format_for_codec(src_sample_format, codec))
    .unwrap_or_else(|| if dst_audio_format == AudioFormat::Decoded {
      decoded_sample_format(src_sample_format)
    } else {
      audio_format_to_sample_format(dst_audio_format)
    });
  let requested_rate = params.samplerate.unwrap_or(src_sample_rate);
  let sample_rate = codec
    .map(|codec| sample_rate_for_codec(requested_rate, codec))
    .unwrap_or(requested_rate);
  // Report exactly the input's interval when the rate is unchanged.
  let sample_interval = if sample_rate == src_sample_rate && !input.sample_interval.is_zero() {
    input.sample_interval
  } else {
    samples_to_duration(1, sample_rate)
  };
  OutputConfig { sample_format, sample_rate, sample_interval, codec_id, bitrate: params.bitrate.unwrap_or(64_000) }
}

fn default_layout(channels: usize) -> ffi::AVChannelLayout {
  unsafe {
    let mut layout: ffi::AVChannelLayout = std::mem::zeroed();
    ffi::av_channel_layout_default(&mut layout, channels as i32);
    layout
  }
}

/// The run of channels with the same format and sample interval that starts
/// at params.input_index (see AudioConverterParams). An index outside the
/// input gives no channels, which push rejects.
fn get_input_range(params: &AudioConverterParams, input: &dyn AnalogData) -> InputChannels {
  let encoding = input.encoding();
  let kind = |i: i32| (input.analog_format(i), input.sample_interval(i));
  let Some(range) = select_input_channels(params.input_index, input.num_channels(), kind) else {
    return InputChannels {
      range: 0..0,
      names: vec![],
      format: AnalogFormat::Double,
      sample_interval: Duration::ZERO,
      encoding,
    };
  };
  let (format, sample_interval) = kind(range.start);
  let names = range.clone().map(|i| input.name(i).to_string()).collect();
  InputChannels { range, format, sample_interval, names, encoding }
}

/// What push works on, under the input lock: the input's channels, the
/// message times, the AAC parser's input and the raw input's conversion.
/// Resetting replaces it with a fresh state of the next generation.
struct InputState {
  params: AudioConverterParams,
  generation: u64,
  /// Picked by the first message after a reset.
  input: Option<InputChannels>,
  rejection_logged: bool,

  pts: i64,
  pts_to_time: VecDeque<(i64, Duration)>,

  // Encoded input: bytes waiting for the parser, which pull runs.
  parser: *mut ffi::AVCodecParserContext,
  parser_packet: *mut ffi::AVPacket,
  in_buffer: Vec<u8>,
  slice_to_pts: VecDeque<(usize, i64)>,
  num_input_bytes: i64,

  // Raw input, converted here.
  output: Option<OutputConfig>,
  layout: ffi::AVChannelLayout,
  single_layout: ffi::AVChannelLayout,
  multi_sampler: *mut ffi::SwrContext,
  reducer_sampler: *mut ffi::SwrContext,
  single_samplers: Vec<*mut ffi::SwrContext>,
  sample_buffers: Vec<Vec<u8>>,
  sample_times: Vec<Option<Duration>>,
}

// SAFETY: the FFmpeg contexts are only used under the input mutex.
unsafe impl Send for InputState {}

impl InputState {
  fn new(params: AudioConverterParams, generation: u64) -> InputState {
    InputState {
      params,
      generation,
      input: None,
      rejection_logged: false,
      pts: 0,
      pts_to_time: VecDeque::new(),
      parser: std::ptr::null_mut(),
      parser_packet: std::ptr::null_mut(),
      in_buffer: vec![],
      slice_to_pts: VecDeque::new(),
      num_input_bytes: 0,
      output: None,
      layout: unsafe { std::mem::zeroed() },
      single_layout: unsafe { std::mem::zeroed() },
      multi_sampler: std::ptr::null_mut(),
      reducer_sampler: std::ptr::null_mut(),
      single_samplers: vec![],
      sample_buffers: vec![],
      sample_times: vec![],
    }
  }

  fn configure_parser(&mut self, encoding: AnalogEncoding) {
    unsafe {
      let codec_id = encoding_codec(encoding);
      self.parser = ffi::av_parser_init(codec_id as i32);
      assert!(!self.parser.is_null(), "Failed to create parser");
      self.parser_packet = ffi::av_packet_alloc();
      assert!(!self.parser_packet.is_null(), "Failed to create parser_packet");
      self.in_buffer.resize(AV_INPUT_BUFFER_PADDING_SIZE as usize, 0);
    }
  }

  /// Appends an encoded message's bytes for the parser.
  fn queue_encoded(&mut self, time: Duration, bytes: &[u8]) {
    let pts = self.pts;
    self.pts_to_time.push_back((pts, time));
    self.pts += 1;

    let buffer_pos = self.in_buffer.len() - AV_INPUT_BUFFER_PADDING_SIZE as usize;
    self.in_buffer.resize(self.in_buffer.len() + bytes.len(), 0);
    let end = buffer_pos + bytes.len();
    self.in_buffer[buffer_pos..end].copy_from_slice(bytes);
    self.slice_to_pts.push_back((end, pts));
  }

  /// Drops the first `used` bytes of buffered input along with the pts slices
  /// that end inside them.
  fn consume_input(&mut self, used: usize) {
    if used == 0 {
      return;
    }
    self.in_buffer.drain(..used);
    self.num_input_bytes += used as i64;
    while self.slice_to_pts.front().is_some_and(|(end, _)| *end <= used) {
      self.slice_to_pts.pop_front();
    }
    for (end, _) in self.slice_to_pts.iter_mut() {
      *end -= used;
    }
  }

  /// The time of the input message that produced `pts`. Earlier entries
  /// belong to messages that produced no frame of their own and are dropped.
  fn take_time(&mut self, pts: i64) -> Option<Duration> {
    let i = self.pts_to_time.iter().position(|(p, _)| *p == pts)?;
    let time = self.pts_to_time[i].1;
    self.pts_to_time.drain(..=i);
    Some(time)
  }

  /// Chooses the raw input's output format and sets up its resamplers and
  /// the frame pool.
  fn configure_raw(&mut self, pool: &Mutex<FramePool>) {
    let input = self.input.clone().expect("configure_raw without an input");
    let src_sample_format = analog_to_sample_format(input.format);
    let src_sample_rate = interval_to_rate(input.sample_interval, self.params.samplerate);
    let output = choose_output(&self.params, &input, src_sample_format, src_sample_rate);
    let channels = input.range.len();
    self.layout = default_layout(channels);
    self.single_layout = default_layout(1);
    self.sample_buffers = vec![vec![]; channels];
    self.sample_times = vec![None; channels];

    *pool.lock().unwrap() = FramePool::new(FramePoolParams::Audio {
      layout: self.layout,
      format: output.sample_format,
      samplerate: output.sample_rate,
    }, self.generation);

    unsafe {
      let ret = ffi::swr_alloc_set_opts2(
        &mut self.multi_sampler,
        &self.layout, output.sample_format, output.sample_rate,
        &self.layout, src_sample_format, src_sample_rate,
        0, std::ptr::null_mut());
      assert!(ret >= 0, "swr_alloc_set_opts2: {}", av_error_string(ret));
      let ret = ffi::swr_init(self.multi_sampler);
      assert!(ret >= 0, "swr_init: {}", av_error_string(ret));

      self.single_samplers = (0..channels).map(|_| {
        let mut sampler = std::ptr::null_mut();
        let ret = ffi::swr_alloc_set_opts2(
          &mut sampler,
          &self.single_layout, output.sample_format, output.sample_rate,
          &self.single_layout, src_sample_format, src_sample_rate,
          0, std::ptr::null_mut());
        assert!(ret >= 0, "swr_alloc_set_opts2: {}", av_error_string(ret));
        let ret = ffi::swr_init(sampler);
        assert!(ret >= 0, "swr_init: {}", av_error_string(ret));
        sampler
      }).collect();

      // The reducer joins the single-channel samplers' outputs, which are
      // already in the destination format and rate, one plane per channel.
      let reducer_sample_format = ffi::av_get_planar_sample_fmt(output.sample_format);
      let ret = ffi::swr_alloc_set_opts2(
        &mut self.reducer_sampler,
        &self.layout, output.sample_format, output.sample_rate,
        &self.layout, reducer_sample_format, output.sample_rate,
        0, std::ptr::null_mut());
      assert!(ret >= 0, "swr_alloc_set_opts2: {}", av_error_string(ret));
      let ret = ffi::swr_init(self.reducer_sampler);
      assert!(ret >= 0, "swr_init: {}", av_error_string(ret));
    }
    self.output = Some(output);
  }

  /// Converts raw samples into frames of the output format and queues them in
  /// `pool`. The pool is only locked to take and queue frames.
  fn convert_samples(&mut self, pool: &Mutex<FramePool>, multi_sampler: *mut ffi::SwrContext, in_ptrs: &[*const u8], in_counts: &[i32], time: Duration, recursing: bool) {
    let input_interval = self.input.as_ref().map_or(Duration::ZERO, |input| input.sample_interval);
    let output = self.output.expect("convert_samples without an output");
    unsafe {
      if in_counts.iter().sum::<i32>() == 0 {
        return;
      }
      let first_count = in_counts[0];
      let use_multi = recursing || (
        in_counts.iter().all(|c| *c == first_count) && self.sample_buffers.iter().all(|b| b.is_empty()));

      if use_multi {
        let pts = self.pts;
        self.pts_to_time.push_back((pts, time));
        self.pts += 1;

        let out_samples = ffi::swr_get_out_samples(multi_sampler, first_count);
        let frame = pool.lock().unwrap().get_writable(out_samples);
        let converted = ffi::swr_convert(multi_sampler, (*frame).extended_data, out_samples, in_ptrs.as_ptr(), first_count);
        assert!(converted >= 0, "swr_convert: {}", av_error_string(converted));
        (*frame).nb_samples = converted;
        (*frame).pts = pts;
        pool.lock().unwrap().push_pending(frame, self.generation);
      } else {
        // Buffers hold output-rate samples in the destination format; all
        // counts below are in samples and converted to bytes only to index.
        let out_bps = ffi::av_get_bytes_per_sample(output.sample_format) as usize;
        let out_rate = output.sample_rate;
        for ui in 0..self.sample_buffers.len() {
          let in_samples = in_counts[ui];
          if in_samples == 0 {
            continue;
          }
          let sampler = self.single_samplers[ui];
          let out_samples = ffi::swr_get_out_samples(sampler, in_samples);
          assert!(out_samples >= 0, "swr_get_out_samples: {}", av_error_string(out_samples));

          let buffer = &mut self.sample_buffers[ui];
          let old_length = buffer.len();
          buffer.resize(old_length + out_samples as usize * out_bps, 0);
          let mut out_ptr = buffer[old_length..].as_mut_ptr();
          let ret = ffi::swr_convert(sampler, &mut out_ptr, out_samples, &in_ptrs[ui], in_samples);
          assert!(ret >= 0, "swr_convert: {}", av_error_string(ret));
          buffer.truncate(old_length + ret as usize * out_bps);

          // Time of the channel's first buffered sample. `time` is the time
          // of this message's last sample.
          self.sample_times[ui].get_or_insert_with(|| {
            time.saturating_sub(input_interval * (in_samples - 1) as u32)
          });
        }

        let Some(latest) = self.sample_times.iter().flatten().max().copied() else {
          return;
        };

        // Samples before the latest-starting channel's first sample can't be
        // lined up with it, so each channel skips that many.
        let starts: Vec<usize> = self.sample_times.iter().map(|t| match t {
          Some(t) => duration_to_samples(latest - *t, out_rate) as usize,
          None => 0,
        }).collect();

        let min_samples = self.sample_buffers.iter().zip(&starts)
          .map(|(b, start)| (b.len() / out_bps).saturating_sub(*start))
          .min()
          .unwrap_or(0);
        if min_samples > 0 {
          let ptrs: Vec<*const u8> = self.sample_buffers.iter().zip(&starts)
            .map(|(b, start)| b[start * out_bps..].as_ptr())
            .collect();
          let counts = vec![min_samples as i32; ptrs.len()];
          // Recursing doesn't touch sample_buffers, so ptrs stay valid.
          self.convert_samples(pool, self.reducer_sampler, &ptrs, &counts, time, true);
        }

        for ((buffer, start), sample_time) in self.sample_buffers.iter_mut().zip(&starts).zip(self.sample_times.iter_mut()) {
          let discarded = (start + min_samples).min(buffer.len() / out_bps);
          buffer.drain(..discarded * out_bps);
          if let Some(t) = sample_time {
            *t += samples_to_duration(discarded as u64, out_rate);
          }
        }
      }
    }
  }
}

impl Drop for InputState {
  fn drop(&mut self) {
    unsafe {
      if !self.parser.is_null() {
        ffi::av_parser_close(self.parser);
      }
      if !self.parser_packet.is_null() {
        ffi::av_packet_free(&mut self.parser_packet);
      }
      if !self.multi_sampler.is_null() {
        ffi::swr_free(&mut self.multi_sampler);
      }
      if !self.reducer_sampler.is_null() {
        ffi::swr_free(&mut self.reducer_sampler);
      }
      for sampler in self.single_samplers.iter_mut() {
        ffi::swr_free(sampler);
      }
      ffi::av_channel_layout_uninit(&mut self.layout);
      ffi::av_channel_layout_uninit(&mut self.single_layout);
    }
  }
}

/// What pull works on, under the codec lock, which push never takes: the
/// decoder, the encoder and what feeds them. Rebuilt whenever the input
/// side's generation changes.
pub struct CodecState {
  generation: u64,
  /// Copied from the input side, for AudioOutput.
  input: Option<InputChannels>,
  output: Option<OutputConfig>,
  layout: ffi::AVChannelLayout,

  // Encoded input
  decoder: Option<*mut ffi::AVCodecContext>,
  decoder_frame: *mut ffi::AVFrame,
  /// The next parsed packet, copied out of the parser's buffer so decoding
  /// doesn't hold the input lock.
  packet_in: *mut ffi::AVPacket,
  decoded_sampler: *mut ffi::SwrContext,

  // Encoded output
  encoder: Option<*mut ffi::AVCodecContext>,
  fifo: *mut ffi::AVAudioFifo,
  fifo_frame: *mut ffi::AVFrame,
  packet: *mut ffi::AVPacket,
  encoder_pts: i64,
  out_buffer: Vec<u8>,
}

// SAFETY: the FFmpeg contexts are only used under the codec mutex.
unsafe impl Send for CodecState {}

impl CodecState {
  fn new(generation: u64) -> CodecState {
    CodecState {
      generation,
      input: None,
      output: None,
      layout: unsafe { std::mem::zeroed() },
      decoder: None,
      decoder_frame: std::ptr::null_mut(),
      packet_in: std::ptr::null_mut(),
      decoded_sampler: std::ptr::null_mut(),
      encoder: None,
      fifo: std::ptr::null_mut(),
      fifo_frame: std::ptr::null_mut(),
      packet: std::ptr::null_mut(),
      encoder_pts: 0,
      out_buffer: vec![],
    }
  }

  fn open_decoder(&mut self) -> *mut ffi::AVCodecContext {
    if let Some(decoder) = self.decoder {
      return decoder;
    }
    let input = self.input.as_ref().expect("open_decoder without an input");
    unsafe {
      let codec_id = encoding_codec(input.encoding);
      let codec = ffi::avcodec_find_decoder(codec_id);
      if codec.is_null() {
        panic!("no {:?} decoder in this FFmpeg build", codec_id);
      }
      let mut context = ffi::avcodec_alloc_context3(codec);
      assert!(!context.is_null(), "avcodec_alloc_context3 failed");
      (*context).pkt_timebase = encoder_time_base(input.sample_interval);
      (*context).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
      let ret = ffi::avcodec_open2(context, codec, std::ptr::null_mut());
      if ret < 0 {
        ffi::avcodec_free_context(&mut context);
        panic!("opening {:?} decoder failed: {}", codec_id, av_error_string(ret));
      }
      self.decoder = Some(context);
      self.decoder_frame = ffi::av_frame_alloc();
      self.packet_in = ffi::av_packet_alloc();
      assert!(!self.decoder_frame.is_null() && !self.packet_in.is_null(), "allocating decoder frame and packet failed");
      context
    }
  }

  fn open_encoder(&mut self, codec_id: ffi::AVCodecID, output: &OutputConfig) -> *mut ffi::AVCodecContext {
    if let Some(encoder) = self.encoder {
      return encoder;
    }
    let channels = self.input.as_ref().map_or(0, |input| input.range.len());
    unsafe {
      ffi::av_channel_layout_uninit(&mut self.layout);
      self.layout = default_layout(channels);
      let codec = ffi::avcodec_find_encoder(codec_id);
      if codec.is_null() {
        panic!("no {:?} encoder in this FFmpeg build", codec_id);
      }
      let mut context = ffi::avcodec_alloc_context3(codec);
      assert!(!context.is_null(), "avcodec_alloc_context3 failed");
      (*context).sample_fmt = output.sample_format;
      (*context).sample_rate = output.sample_rate;
      (*context).time_base = ffi::AVRational { num: 1, den: output.sample_rate };
      (*context).bit_rate = output.bitrate;
      ffi::av_channel_layout_copy(&mut (*context).ch_layout, &self.layout);
      let ret = ffi::avcodec_open2(context, codec, std::ptr::null_mut());
      if ret < 0 {
        ffi::avcodec_free_context(&mut context);
        panic!("opening {:?} encoder failed: {}", codec_id, av_error_string(ret));
      }

      self.fifo = ffi::av_audio_fifo_alloc((*context).sample_fmt, (*context).ch_layout.nb_channels, (*context).frame_size.max(1));
      self.fifo_frame = ffi::av_frame_alloc();
      (*self.fifo_frame).format = (*context).sample_fmt as i32;
      ffi::av_channel_layout_copy(&mut (*self.fifo_frame).ch_layout, &(*context).ch_layout);
      (*self.fifo_frame).sample_rate = (*context).sample_rate;
      (*self.fifo_frame).nb_samples = (*context).frame_size;
      let ret = ffi::av_frame_get_buffer(self.fifo_frame, 0);
      assert!(ret >= 0, "av_frame_get_buffer: {}", av_error_string(ret));
      self.packet = ffi::av_packet_alloc();
      assert!(!self.packet.is_null(), "av_packet_alloc");
      self.encoder_pts = 0;
      self.encoder = Some(context);
      context
    }
  }

  /// Sends `frame`'s samples through the FIFO into the encoder and appends
  /// every packet it produces, ADTS framed, to out_buffer. Frees `frame`.
  /// Returns the number of samples pushed, which the encoder will eventually
  /// output even if it holds them in the FIFO or its own delay for now.
  fn encode_frame(&mut self, encoder: *mut ffi::AVCodecContext, mut frame: *mut ffi::AVFrame) -> i32 {
    unsafe {
      let nb_samples = (*frame).nb_samples;
      let ret = ffi::av_audio_fifo_write(self.fifo, (*frame).extended_data as *const *mut c_void, nb_samples);
      assert!(ret == nb_samples, "av_audio_fifo_write {}", av_error_string(ret));
      ffi::av_frame_free(&mut frame);

      let frame_size = (*encoder).frame_size;
      while ffi::av_audio_fifo_size(self.fifo) >= frame_size {
        let ret = ffi::av_frame_make_writable(self.fifo_frame);
        assert!(ret >= 0, "av_frame_make_writable {}", av_error_string(ret));

        (*self.fifo_frame).nb_samples = frame_size;
        let read = ffi::av_audio_fifo_read(self.fifo, (*self.fifo_frame).extended_data as *const *mut c_void, frame_size);
        assert!(read == frame_size, "av_audio_fifo_read {}", av_error_string(read));
        (*self.fifo_frame).pts = self.encoder_pts;
        self.encoder_pts += i64::from(frame_size);

        let ret = ffi::avcodec_send_frame(encoder, self.fifo_frame);
        assert!(ret >= 0, "avcodec_send_frame: {}", av_error_string(ret));
        // Drain after every send, or the next send can return EAGAIN.
        self.receive_packets(encoder);
      }
      nb_samples
    }
  }

  fn receive_packets(&mut self, encoder: *mut ffi::AVCodecContext) {
    unsafe {
      loop {
        let ret = ffi::avcodec_receive_packet(encoder, self.packet);
        if ret == AVERROR_EAGAIN || ret == AVERROR_EOF {
          break;
        }
        assert!(ret >= 0, "Error during encoding: {}", av_error_string(ret));

        let size = (*self.packet).size as usize;
        let header = adts_header(size, (*encoder).sample_rate, (*encoder).ch_layout.nb_channels);
        self.out_buffer.extend_from_slice(&header);
        self.out_buffer.extend_from_slice(std::slice::from_raw_parts((*self.packet).data, size));
        ffi::av_packet_unref(self.packet);
      }
    }
  }
}

impl Drop for CodecState {
  fn drop(&mut self) {
    unsafe {
      if let Some(mut decoder) = self.decoder.take() {
        ffi::avcodec_free_context(&mut decoder);
      }
      if let Some(mut encoder) = self.encoder.take() {
        ffi::avcodec_free_context(&mut encoder);
      }
      if !self.fifo.is_null() {
        ffi::av_audio_fifo_free(self.fifo);
      }
      ffi::av_frame_free(&mut self.decoder_frame);
      ffi::av_frame_free(&mut self.fifo_frame);
      ffi::av_packet_free(&mut self.packet_in);
      ffi::av_packet_free(&mut self.packet);
      if !self.decoded_sampler.is_null() {
        ffi::swr_free(&mut self.decoded_sampler);
      }
      ffi::av_channel_layout_uninit(&mut self.layout);
    }
  }
}

/// Converts analog data to a target audio format. It's Sync: push and pull
/// can run on different threads at once. push only takes the input lock (and
/// the frame pool's, briefly); pull holds the codec lock for its decoding and
/// encoding and takes the input and pool locks only to parse one packet or
/// take one frame. Raw input is converted in push, encoded input decoded in
/// pull.
///
/// Locks are always taken in the order codec, input, pool. A reset from the
/// push side bumps the input generation; pull rebuilds its codec state when
/// it sees the change.
pub struct AudioConverter {
  api: ThalamusAPIThreadSafe,
  codec: Mutex<CodecState>,
  input: Mutex<InputState>,
  pool: Mutex<FramePool>,
}

impl AudioConverter {
  pub fn new(api: ThalamusAPIThreadSafe, params: AudioConverterParams) -> AudioConverter {
    AudioConverter {
      api,
      codec: Mutex::new(CodecState::new(0)),
      input: Mutex::new(InputState::new(params, 0)),
      pool: Mutex::new(FramePool::new(FramePoolParams::empty(), 0)),
    }
  }

  /// Starts over with `params`; pull picks it up without push waiting for it.
  pub fn reconfigure(&self, params: AudioConverterParams) {
    let mut input = self.input.lock().unwrap();
    self.reset(&mut input, params);
  }

  /// Replaces the input state and frame pool with ones of the next
  /// generation; pull rebuilds its codec state when it sees it.
  fn reset(&self, input: &mut InputState, params: AudioConverterParams) {
    let generation = input.generation + 1;
    *input = InputState::new(params, generation);
    *self.pool.lock().unwrap() = FramePool::new(FramePoolParams::empty(), generation);
  }

  /// Queues `data`'s analog data for conversion. Raw input is converted here;
  /// encoded input is queued for the parser.
  pub fn push(&self, data: &dyn NodeData) {
    let Some(analog) = data.analog() else {
      return;
    };
    let mut guard = self.input.lock().unwrap();
    // The input channels are picked once, so a change starts over with the
    // current parameters; pull picks up the new generation.
    if analog.channels_changed() {
      let params = guard.params;
      self.reset(&mut guard, params);
    }
    let state = &mut *guard;

    if state.input.is_none() {
      let input_channels = get_input_range(&state.params, analog);
      if input_channels.range.is_empty() || input_channels.sample_interval.is_zero() || !is_supported_input(&input_channels) {
        if !state.rejection_logged {
          if input_channels.range.is_empty() {
            println!(
              "AudioConverter: Audio Index {} is outside the input's {} channels",
              state.params.input_index, analog.num_channels());
          } else {
            println!(
              "AudioConverter can't convert {:?} ({:?}) input with a {:?} sample interval",
              input_channels.format, input_channels.encoding, input_channels.sample_interval);
          }
        }
        state.rejection_logged = true;
        return;
      }
      if input_channels.format == AnalogFormat::Encoded {
        state.configure_parser(input_channels.encoding);
      }
      state.input = Some(input_channels);
    }

    let (encoded, range) = {
      let input = state.input.as_ref().unwrap();
      (input.format == AnalogFormat::Encoded, input.range.clone())
    };
    if encoded {
      state.queue_encoded(data.time(), analog.buffer());
      return;
    }

    if state.output.is_none() {
      state.configure_raw(&self.pool);
    }
    let pointers: Vec<_> = range.clone().map(|i| analog_data_ptr(analog, i)).collect();
    let counts: Vec<_> = range.map(|i| analog.count(i) as i32).collect();
    let multi_sampler = state.multi_sampler;
    state.convert_samples(&self.pool, multi_sampler, &pointers, &counts, data.time(), false);
  }

  /// Parses buffered input until one packet is ready and sends it to the
  /// decoder. The input lock is only held to parse: the packet is copied out
  /// first, and decoding (which avcodec_send_packet starts) runs without it.
  /// Returns false when no complete packet is buffered or the input side was
  /// reset. Only called after avcodec_receive_frame returned EAGAIN, so the
  /// decoder accepts the packet.
  fn send_next_packet(&self, codec: &mut CodecState, decoder: *mut ffi::AVCodecContext) -> bool {
    let padding = AV_INPUT_BUFFER_PADDING_SIZE as usize;
    unsafe {
      {
        let mut input = self.input.lock().unwrap();
        if input.generation != codec.generation {
          return false;
        }
        loop {
          let available = input.in_buffer.len().saturating_sub(padding);
          if available == 0 {
            return false;
          }
          let pts = input.slice_to_pts.front().map_or(ffi::AV_NOPTS_VALUE, |(_, pts)| *pts);
          let used = ffi::av_parser_parse2(
            input.parser, decoder,
            &mut (*input.parser_packet).data,
            &mut (*input.parser_packet).size,
            input.in_buffer.as_ptr(),
            available as i32,
            pts, ffi::AV_NOPTS_VALUE,
            input.num_input_bytes
          );
          assert!(used >= 0, "av_parser_parse2: {}", av_error_string(used));

          // The parsed packet can point into in_buffer, so it's copied before
          // consume_input shifts the buffer.
          let got_packet = (*input.parser_packet).size > 0;
          if got_packet {
            (*input.parser_packet).pts = (*input.parser).pts;
            let ret = ffi::av_packet_ref(codec.packet_in, input.parser_packet);
            assert!(ret >= 0, "av_packet_ref: {}", av_error_string(ret));
          }
          input.consume_input(used as usize);

          if got_packet {
            break;
          }
          if used == 0 {
            return false;
          }
        }
      }

      let ret = ffi::avcodec_send_packet(decoder, codec.packet_in);
      ffi::av_packet_unref(codec.packet_in);
      if ret < 0 {
        // A corrupt packet loses its audio but shouldn't stop the stream.
        println!("AudioConverter: dropping undecodable packet: {}", av_error_string(ret));
      }
      true
    }
  }

  /// The next decoded frame in the output format, or None when more input is
  /// needed.
  fn next_decoded_frame(&self, codec: &mut CodecState) -> Option<*mut ffi::AVFrame> {
    let decoder = codec.open_decoder();
    unsafe {
      loop {
        let ret = ffi::avcodec_receive_frame(decoder, codec.decoder_frame);
        if ret == AVERROR_EAGAIN {
          if !self.send_next_packet(codec, decoder) {
            return None;
          }
          continue;
        }
        if ret == AVERROR_EOF {
          return None;
        }
        assert!(ret >= 0, "avcodec_receive_frame: {}", av_error_string(ret));
        break;
      }

      let decoder_frame = codec.decoder_frame;
      let src_sample_format = std::mem::transmute::<i32, ffi::AVSampleFormat>((*decoder_frame).format);
      let src_sample_rate = (*decoder_frame).sample_rate;
      let nb_samples = (*decoder_frame).nb_samples;

      // The output format depends on what the decoder produces, so it's
      // chosen here, from the first decoded frame.
      if codec.output.is_none() {
        let params = self.input.lock().unwrap().params;
        let input = codec.input.clone().expect("decoding without an input");
        let output = choose_output(&params, &input, src_sample_format, src_sample_rate);
        ffi::av_channel_layout_uninit(&mut codec.layout);
        codec.layout = default_layout(input.range.len());
        let ret = ffi::swr_alloc_set_opts2(
          &mut codec.decoded_sampler,
          &codec.layout, output.sample_format, output.sample_rate,
          &codec.layout, src_sample_format, src_sample_rate,
          0, std::ptr::null_mut());
        assert!(ret >= 0, "swr_alloc_set_opts2: {}", av_error_string(ret));
        let ret = ffi::swr_init(codec.decoded_sampler);
        assert!(ret >= 0, "swr_init: {}", av_error_string(ret));
        {
          let mut pool = self.pool.lock().unwrap();
          if pool.generation != codec.generation {
            ffi::av_frame_unref(decoder_frame);
            return None;
          }
          *pool = FramePool::new(FramePoolParams::Audio {
            layout: codec.layout,
            format: output.sample_format,
            samplerate: output.sample_rate,
          }, codec.generation);
        }
        codec.output = Some(output);
      }
      let output = codec.output.unwrap();

      // Already in the output format: hand out a new reference to the
      // decoded buffers instead of converting. decoder_frame is reused by the
      // next avcodec_receive_frame, so it can't be handed out itself.
      let same_layout = ffi::av_channel_layout_compare(&(*decoder_frame).ch_layout, &codec.layout) == 0;
      if same_layout && src_sample_format == output.sample_format && src_sample_rate == output.sample_rate {
        let frame = ffi::av_frame_clone(decoder_frame);
        assert!(!frame.is_null(), "av_frame_clone failed");
        ffi::av_frame_unref(decoder_frame);
        return Some(frame);
      }

      let out_samples = ffi::swr_get_out_samples(codec.decoded_sampler, nb_samples);
      assert!(out_samples >= 0, "swr_get_out_samples: {}", av_error_string(out_samples));
      let frame = self.pool.lock().unwrap().get_writable(out_samples);
      let converted = ffi::swr_convert(
        codec.decoded_sampler,
        (*frame).extended_data,
        out_samples,
        (*decoder_frame).extended_data as *const *const u8,
        nb_samples);
      assert!(converted >= 0, "swr_convert: {}", av_error_string(converted));
      (*frame).nb_samples = converted;
      (*frame).pts = (*decoder_frame).pts;
      ffi::av_frame_unref(decoder_frame);

      let mut pool = self.pool.lock().unwrap();
      if !pool.push_pending(frame, codec.generation) {
        return None;
      }
      pool.get_pending(codec.generation)
    }
  }

  /// The next converted message: one per frame. Encoded outputs are emitted
  /// even when the encoder produced no packets yet: the buffer is empty and
  /// encoded_count is the number of samples pushed.
  pub fn pull(&self) -> Option<AudioOutput<'_>> {
    let mut codec = self.codec.lock().unwrap();

    // Catch up with the input side: a reset there starts the codec state
    // over, and raw input's output format is chosen there.
    let encoded = {
      let input = self.input.lock().unwrap();
      if codec.generation != input.generation {
        *codec = CodecState::new(input.generation);
      }
      let input_channels = input.input.as_ref()?;
      if codec.input.is_none() {
        codec.input = Some(input_channels.clone());
      }
      let encoded = input_channels.format == AnalogFormat::Encoded;
      if !encoded && codec.output.is_none() {
        codec.output = input.output;
      }
      encoded
    };

    let frame = if encoded {
      self.next_decoded_frame(&mut codec)?
    } else {
      self.pool.lock().unwrap().get_pending(codec.generation)?
    };
    let pts = unsafe { (*frame).pts };
    let time = self.input.lock().unwrap().take_time(pts).unwrap_or_else(|| self.api.time());

    let output = codec.output.expect("a converted frame without an output format");
    let Some(codec_id) = output.codec_id else {
      return Some(AudioOutput { codec, frame: Some(frame), time, encoded_count: 0 });
    };
    let encoder = codec.open_encoder(codec_id, &output);
    codec.out_buffer.clear();
    let encoded_count = codec.encode_frame(encoder, frame);
    Some(AudioOutput { codec, frame: None, time, encoded_count })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn decoded_samples_keep_their_format_when_thalamus_has_it() {
    use ffi::AVSampleFormat::*;
    assert_eq!(decoded_sample_format(AV_SAMPLE_FMT_S16), AV_SAMPLE_FMT_S16P);
    assert_eq!(decoded_sample_format(AV_SAMPLE_FMT_S32P), AV_SAMPLE_FMT_S32P);
    assert_eq!(decoded_sample_format(AV_SAMPLE_FMT_DBL), AV_SAMPLE_FMT_DBLP);
    // AAC decodes to floats, which Thalamus has no format for.
    assert_eq!(decoded_sample_format(AV_SAMPLE_FMT_FLTP), AV_SAMPLE_FMT_DBLP);
  }

  #[test]
  fn interval_to_rate_recovers_common_rates_from_rounded_or_truncated_intervals() {
    // 1e9 / 44100 = 22675.73 ns
    assert_eq!(interval_to_rate(Duration::from_nanos(22675), None), 44100);
    assert_eq!(interval_to_rate(Duration::from_nanos(22676), None), 44100);
    // 1e9 / 48000 = 20833.33 ns
    assert_eq!(interval_to_rate(Duration::from_nanos(20833), None), 48000);
    assert_eq!(interval_to_rate(Duration::from_nanos(20834), None), 48000);
    assert_eq!(interval_to_rate(Duration::from_micros(125), None), 8000);
  }

  #[test]
  fn interval_to_rate_prefers_the_output_rate() {
    // 1e9 / 30000 = 33333.33 ns isn't a common rate
    assert_eq!(interval_to_rate(Duration::from_nanos(33333), Some(30000)), 30000);
    assert_eq!(interval_to_rate(Duration::from_nanos(33333), None), 30000);
    // 1e9 / 44101 = 22675.22 ns: 22675 is within 1 ns of both, the preferred rate wins
    assert_eq!(interval_to_rate(Duration::from_nanos(22675), Some(44101)), 44101);
  }

  #[test]
  fn interval_to_rate_rounds_unknown_rates() {
    // 1e9 / 50 us = 20000 Hz exactly, not a listed rate
    assert_eq!(interval_to_rate(Duration::from_micros(50), None), 20000);
    // 1e9 / 30001 ns = 33332.22 Hz
    assert_eq!(interval_to_rate(Duration::from_nanos(30001), None), 33332);
  }

  #[test]
  fn sample_durations_dont_accumulate_interval_rounding() {
    // An hour of 44.1 kHz is exactly 3600 s; summing a 22676 ns interval
    // per sample would be about 11 ms long.
    assert_eq!(samples_to_duration(44_100 * 3600, 44100), Duration::from_secs(3600));
    assert_eq!(samples_to_duration(1, 44100), Duration::from_nanos(22676));
    assert_eq!(duration_to_samples(Duration::from_secs(3600), 44100), 44_100 * 3600);
    assert_eq!(duration_to_samples(Duration::from_nanos(22675), 44100), 1);
  }

  #[test]
  fn audio_converter_can_be_shared_between_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AudioConverter>();
  }

  #[test]
  fn input_channels_are_selected_from_the_audio_index() {
    // A media converter's AAC output: two stats channels, then two audio channels.
    let kinds = ['s', 's', 'a', 'a'];
    let select = |index| select_input_channels(index, 4, |i| kinds[i as usize]);
    // Negative indexes count from the end and go backwards.
    assert_eq!(select(-1), Some(2..4));
    assert_eq!(select(-2), Some(2..3));
    assert_eq!(select(-3), Some(0..2));
    assert_eq!(select(-4), Some(0..1));
    // Non-negative indexes count from the start and go forwards.
    assert_eq!(select(0), Some(0..2));
    assert_eq!(select(1), Some(1..2));
    assert_eq!(select(2), Some(2..4));
    assert_eq!(select(3), Some(3..4));
    // Outside the channels.
    assert_eq!(select(4), None);
    assert_eq!(select(-5), None);
    assert_eq!(select_input_channels(-1, 0, |i| kinds[i as usize]), None);
  }
}
