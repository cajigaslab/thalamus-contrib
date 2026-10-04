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
use std::time::Duration;

use ffmpeg_sys_next::AVCodecConfig::{AV_CODEC_CONFIG_SAMPLE_FORMAT, AV_CODEC_CONFIG_SAMPLE_RATE};
use ffmpeg_sys_next::{self as ffi, AV_INPUT_BUFFER_PADDING_SIZE, EAGAIN};

use crate::api::{AnalogData, AnalogEncoding, AnalogFormat, NodeData, ThalamusAPI, ThalamusAPIThreadSafe};
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

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AudioConverterParams {
  pub format: Option<AudioFormat>,
  pub bitrate: Option<i64>,
  pub input_index: Option<usize>,
  pub samplerate: Option<i32>
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AudioFormat {
  Integer,
  Decimal,
  AAC,
}


/// One converted analog message.
pub struct AudioOutput<'a> {
  converter: &'a AudioConverter,
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

  fn is_short_data(&self) -> bool {
    self.av_format() == ffi::AVSampleFormat::AV_SAMPLE_FMT_S16P
  }

  /// AAC output reports its channels (names and sample intervals) with no
  /// samples; the audio is in the buffer.
  fn num_channels(&self) -> i32 {
    self.converter.input_channels.range.len() as i32
  }

  fn sample_interval(&self, _channel: i32) -> Duration {
    self.converter.dst_sample_interval
  }

  fn name(&self, channel: i32) -> &str {
    self.converter.input_channels.names[channel as usize].as_str()
  }

  fn buffer(&self) -> &[u8] {
    self.converter.out_buffer.as_slice()
  }

  fn encoding(&self) -> AnalogEncoding {
    if let Some(encoder) = self.converter.encoder {
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

/// How many samples per channel an audio frame's buffers can hold.
unsafe fn frame_capacity(frame: *const ffi::AVFrame) -> i32 {
  unsafe {
    if (*frame).buf[0].is_null() {
      return 0;
    }
    let format: ffi::AVSampleFormat = std::mem::transmute((*frame).format);
    let bps = ffi::av_get_bytes_per_sample(format);
    let bytes_per_sample = if ffi::av_sample_fmt_is_planar(format) != 0 {
      bps
    } else {
      bps * (*frame).ch_layout.nb_channels
    };
    if bytes_per_sample <= 0 {
      0
    } else {
      (*frame).linesize[0] / bytes_per_sample
    }
  }
}

enum FramePoolParams {
  Audio{layout: ffi::AVChannelLayout, format: ffi::AVSampleFormat, samplerate: i32}
}

struct FramePool {
  writable: VecDeque<*mut ffi::AVFrame>,
  pending: VecDeque<*mut ffi::AVFrame>,
  used: VecDeque<*mut ffi::AVFrame>,
  params: FramePoolParams
}

impl FramePool {
  fn new(params: FramePoolParams) -> FramePool {
    FramePool {
      params,
      writable: VecDeque::new(),
      pending: VecDeque::new(),
      used: VecDeque::new(),
    }
  }

  fn get_writable(&mut self, nb_samples: i32) -> *mut ffi::AVFrame {
    unsafe {
      self.used.retain(|f| {
        let writable = ffi::av_frame_is_writable(*f) != 0;
        if writable {
          self.writable.push_back(*f);
        }
        !writable
      });

      // A recycled frame keeps its buffers, so reuse it when they're big enough
      // and replace it otherwise (av_frame_get_buffer on a frame that has
      // buffers leaks them).
      if let Some(mut frame) = self.writable.pop_front() {
        if frame_capacity(frame) >= nb_samples {
          (*frame).nb_samples = nb_samples;
          return frame;
        }
        ffi::av_frame_free(&mut frame);
      }

      let frame = ffi::av_frame_alloc();
      assert!(!frame.is_null(), "av_frame_alloc failed");
      match self.params {
        FramePoolParams::Audio { layout, format, samplerate } => {
          ffi::av_channel_layout_copy(&mut (*frame).ch_layout, &layout);
          (*frame).format = format as i32;
          (*frame).sample_rate = samplerate;
          (*frame).nb_samples = nb_samples;
        }
      };

      let ret = ffi::av_frame_get_buffer(frame, 0);
      assert!(ret >= 0, "ffi::av_frame_get_buffer: {}", av_error_string(ret));
      frame
    }
  }

  fn push_pending(&mut self, frame: *mut ffi::AVFrame) {
    self.pending.push_back(frame);
  }

  fn get_pending(&mut self) -> Option<*mut ffi::AVFrame> {
    let result = self.pending.pop_front();
    result.map(|r| {
      let new_ref = unsafe { ffi::av_frame_clone(r) };
      self.used.push_back(r);
      new_ref
    })
  }
}

impl Drop for FramePool {
  fn drop(&mut self) {
    unsafe {
      for f in self.writable.iter_mut() {
        ffi::av_frame_free(f);
      }
      for f in self.pending.iter_mut() {
        ffi::av_frame_free(f);
      }
      for f in self.used.iter_mut() {
        ffi::av_frame_free(f);
      }
    }
  }
}

/// Converts analog data to a target audio format. Like the image Converter,
/// push() only copies the input; the conversion happens in pull(), so it
/// runs wherever pull() is called.
pub struct AudioConverter {
  params: AudioConverterParams,
  api: ThalamusAPIThreadSafe,

  multi_sampler: *mut ffi::SwrContext,//
  reducer_sampler: *mut ffi::SwrContext,//
  single_samplers: Vec<*mut ffi::SwrContext>,//

  parser: *mut ffi::AVCodecParserContext,//
  encoder: Option<*mut ffi::AVCodecContext>,//
  decoder: Option<*mut ffi::AVCodecContext>,//

  decoder_frame: *mut ffi::AVFrame,//
  fifo_frame: *mut ffi::AVFrame,//
  
  needs_conversion: bool,
  
  pts: i64,//
  pts_to_time: VecDeque<(i64, Duration)>,//
  encoder_pts: i64,
  
  in_buffer: Vec<u8>,//
  out_buffer: Vec<u8>,//
  slice_to_pts: VecDeque<(usize, i64)>,//

  dst_frame_pool: FramePool,//

  dst_sample_format: ffi::AVSampleFormat,//
  dst_sample_rate: i32,//
  dst_sample_interval: Duration,//
  fifo: *mut ffi::AVAudioFifo,//
  layout: ffi::AVChannelLayout,//
  single_layout: ffi::AVChannelLayout,//

  packet: *mut ffi::AVPacket,//
  parser_packet: *mut ffi::AVPacket,//
  decoder_configured: bool,//
  encoder_configured: bool,//
  sampler_configured: bool,//
  input_channels: InputChannels,//

  sample_buffers: Vec<Vec<u8>>,//
  sample_times: Vec<Option<Duration>>,//
  num_input_bytes: i64,//
  need_key_frame: bool,//
  rejection_logged: bool,
}

// SAFETY: the FFmpeg contexts are only used through &mut self, and FFmpeg
// codec contexts may move between threads as long as they aren't used
// concurrently.
unsafe impl Send for AudioConverter {}

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
  }
}

fn audio_format_to_sample_format(format: AudioFormat) -> ffi::AVSampleFormat {
  match format {
    AudioFormat::Integer => ffi::AVSampleFormat::AV_SAMPLE_FMT_S16P,
    AudioFormat::Decimal => ffi::AVSampleFormat::AV_SAMPLE_FMT_DBLP,
    _ => panic!("audio_format_to_sample_format {:?}", format),
  }
}

/// The codec's supported values for `config`. Empty means it accepts anything.
fn get_codec_config<T>(codec: *const ffi::AVCodec, config: ffi::AVCodecConfig) -> &'static [T] {
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

/// Whether the trailing channel run of an input can be converted.
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

impl AudioConverter {
  pub fn new(api: ThalamusAPIThreadSafe, params: AudioConverterParams) -> AudioConverter {
    AudioConverter {
      params,
      api,

      multi_sampler: std::ptr::null_mut(),
      reducer_sampler: std::ptr::null_mut(),
      single_samplers: vec![],

      parser: std::ptr::null_mut(),
      encoder: None,
      decoder: None,

      decoder_frame: std::ptr::null_mut(),

      fifo: std::ptr::null_mut(),
      fifo_frame: std::ptr::null_mut(),
      
      pts: 0,
      pts_to_time: VecDeque::new(),
      encoder_pts: 0,

      in_buffer: vec![],
      out_buffer: vec![],
      slice_to_pts: VecDeque::new(),
      rejection_logged: false,

      dst_sample_format: ffi::AVSampleFormat::AV_SAMPLE_FMT_NONE,
      dst_sample_rate: -1,
      dst_sample_interval: Duration::ZERO,

      packet: std::ptr::null_mut(),
      parser_packet: std::ptr::null_mut(),

      sample_buffers: vec![],
      sample_times: vec![],
      need_key_frame: false,

      layout: unsafe { std::mem::zeroed() },
      single_layout: unsafe { std::mem::zeroed() },
      decoder_configured: false,
      encoder_configured: false,
      sampler_configured: false,
      input_channels: InputChannels { range: 0..0, format: AnalogFormat::Double, encoding: AnalogEncoding::None, sample_interval: Duration::default(), names: vec![] },
      num_input_bytes: 0,
      dst_frame_pool: FramePool::new(
        FramePoolParams::Audio { 
          layout: unsafe { std::mem::zeroed() }, 
          format: ffi::AVSampleFormat::AV_SAMPLE_FMT_NONE, 
          samplerate: 0,
        }),
      needs_conversion: true,
    }
  }

  pub fn channels_changed(&mut self) {
    self.cleanup();
  }

  fn cleanup(&mut self) {
    self.decoder_configured = false;
    self.encoder_configured = false;
    self.sampler_configured = false;
    self.rejection_logged = false;
    self.needs_conversion = true;
    unsafe {
      ffi::av_channel_layout_uninit(&mut self.layout);
      ffi::av_channel_layout_uninit(&mut self.single_layout);

      if let Some(mut decoder) = self.decoder {
        ffi::avcodec_free_context(&mut decoder);
        ffi::av_parser_close(self.parser);
        self.parser = std::ptr::null_mut();
        self.decoder = None;
      }
      if let Some(mut encoder) = self.encoder {
        ffi::avcodec_free_context(&mut encoder);
        ffi::av_audio_fifo_free(self.fifo);
        self.fifo = std::ptr::null_mut();
        self.encoder = None;
      }

      if self.decoder_frame != std::ptr::null_mut() {
        ffi::av_frame_free(&mut self.decoder_frame);
        self.decoder_frame = std::ptr::null_mut();
      }
      if self.fifo_frame != std::ptr::null_mut() {
        ffi::av_frame_free(&mut self.fifo_frame);
        self.fifo_frame = std::ptr::null_mut();
      }

      if self.packet != std::ptr::null_mut() {
        ffi::av_packet_free(&mut self.packet);
        self.packet = std::ptr::null_mut();
      }
      if self.parser_packet != std::ptr::null_mut() {
        ffi::av_packet_free(&mut self.parser_packet);
        self.parser_packet = std::ptr::null_mut();
      }
      
      if self.multi_sampler != std::ptr::null_mut() {
        ffi::swr_free(&mut self.multi_sampler);
      }
      if self.reducer_sampler != std::ptr::null_mut() {
        ffi::swr_free(&mut self.reducer_sampler);
      }
      for mut sampler in std::mem::take(&mut self.single_samplers) {
        ffi::swr_free(&mut sampler);
      }
      self.pts = 0;
      self.pts_to_time.clear();
      self.encoder_pts = 0;
      self.in_buffer.clear();
      self.out_buffer.clear();
      self.slice_to_pts.clear();
      self.sample_buffers.clear();
      self.sample_times.clear();
      self.need_key_frame = false;
      // Pending frames are in the old output format.
      self.dst_frame_pool = FramePool::new(FramePoolParams::Audio {
        layout: std::mem::zeroed(),
        format: ffi::AVSampleFormat::AV_SAMPLE_FMT_NONE,
        samplerate: 0,
      });
    }
  }

  fn configure_decoder(&mut self, input_channels: InputChannels) {
    if self.decoder_configured {
      return;
    }
    self.decoder_configured = true;
    let src_encoding = input_channels.encoding;
    unsafe {
      ffi::av_channel_layout_default(&mut self.layout, input_channels.range.len() as i32);
      ffi::av_channel_layout_default(&mut self.single_layout, 1);

      let time_base = encoder_time_base(input_channels.sample_interval);
      self.num_input_bytes = 0;

      if input_channels.format == AnalogFormat::Encoded {
        let codec_id = encoding_codec(src_encoding);
        let codec = ffi::avcodec_find_decoder(codec_id);
        if codec.is_null() {
          panic!("no {:?} decoder in this FFmpeg build", codec_id);
        }

        let mut context = ffi::avcodec_alloc_context3(codec);
        if context.is_null() {
          panic!("avcodec_alloc_context3 failed");
        }

        (*context).pkt_timebase = time_base;
        (*context).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;

        let ret = ffi::avcodec_open2(context, codec, std::ptr::null_mut());
        if ret < 0 {
          ffi::avcodec_free_context(&mut context);
          panic!("opening {:?} decoder failed: {}", codec_id, av_error_string(ret));
        }

        self.decoder = Some(context);

        self.parser = ffi::av_parser_init(codec_id as i32);
        self.in_buffer.resize(AV_INPUT_BUFFER_PADDING_SIZE as usize, 0);
        assert!(self.parser != std::ptr::null_mut(), "Failed to create parser");

        self.parser_packet = ffi::av_packet_alloc();
        assert!(self.parser_packet != std::ptr::null_mut(), "Failed to create parser_packet");

        self.decoder_frame = ffi::av_frame_alloc();
        self.needs_conversion = true;
      }

      self.input_channels = input_channels;
    }
  }

  fn configure_encoder(&mut self, src_sample_format: ffi::AVSampleFormat, src_sample_rate: i32) {
    if self.encoder_configured {
      return;
    }
    self.encoder_configured = true;
    let input_channels = &self.input_channels;
    let src_encoding = input_channels.encoding;
    unsafe {
      let src_audio_format = analog_to_audio_format(input_channels.format, src_encoding);

      let dst_audio_format = self.params.format.unwrap_or(src_audio_format);
      let dst_codec_id = audio_format_to_encoding(dst_audio_format);
      let codec = dst_codec_id.map(|codec_id| {
        let codec = ffi::avcodec_find_encoder(codec_id);
        if codec.is_null() {
          panic!("no {:?} encoder in this FFmpeg build", codec_id);
        }
        codec
      });
      let dst_sample_format = codec
        .map(|codec| sample_format_for_codec(src_sample_format, codec))
        .unwrap_or_else(|| audio_format_to_sample_format(dst_audio_format));

      let requested_rate = self.params.samplerate.unwrap_or(src_sample_rate);
      let dst_sample_rate = codec
        .map(|codec| sample_rate_for_codec(requested_rate, codec))
        .unwrap_or(requested_rate);
      self.dst_sample_format = dst_sample_format;
      self.dst_sample_rate = dst_sample_rate;
      // Report exactly the input's interval when the rate is unchanged.
      self.dst_sample_interval = if dst_sample_rate == src_sample_rate && !input_channels.sample_interval.is_zero() {
        input_channels.sample_interval
      } else {
        samples_to_duration(1, dst_sample_rate)
      };
      self.encoder_pts = 0;

      self.encoder = codec.map(|codec| {
        let mut context = ffi::avcodec_alloc_context3(codec);
        if context.is_null() {
          panic!("avcodec_alloc_context3 failed");
        }

        (*context).sample_fmt = dst_sample_format;
        (*context).sample_rate = dst_sample_rate;
        (*context).time_base = ffi::AVRational { num: 1, den: dst_sample_rate };
        (*context).bit_rate = self.params.bitrate.unwrap_or(64_000);
        ffi::av_channel_layout_copy(&mut (*context).ch_layout, &self.layout);

        let ret = ffi::avcodec_open2(context, codec, std::ptr::null_mut());
        if ret < 0 {
          ffi::avcodec_free_context(&mut context);
          panic!("opening {:?} encoder failed: {}", (*codec).id, av_error_string(ret));
        }

        context
      });

      if let Some(encoder) = self.encoder {
        self.fifo = ffi::av_audio_fifo_alloc(
          (*encoder).sample_fmt,
          (*encoder).ch_layout.nb_channels,
          (*encoder).frame_size.max(1),
        );
        self.fifo_frame = ffi::av_frame_alloc();

        (*self.fifo_frame).format = (*encoder).sample_fmt as i32;
        ffi::av_channel_layout_copy(&mut (*self.fifo_frame).ch_layout, &(*encoder).ch_layout);
        (*self.fifo_frame).sample_rate = (*encoder).sample_rate;
        (*self.fifo_frame).nb_samples = (*encoder).frame_size;
        let ret = ffi::av_frame_get_buffer(self.fifo_frame, 0);
        assert!(ret >= 0, "av_frame_get_buffer: {}", av_error_string(ret));

        self.packet = ffi::av_packet_alloc();
        assert!(self.packet != std::ptr::null_mut(), "av_packet_alloc");
      }
    }
  }

  fn configure_resampler(&mut self, src_sample_format: ffi::AVSampleFormat, src_sample_rate: i32) {
    if self.sampler_configured {
      return;
    }
    self.sampler_configured = true;
    self.sample_buffers.clear();
    self.sample_buffers.resize(self.input_channels.range.len(), vec![]);
    self.sample_times.clear();
    self.sample_times.resize(self.input_channels.range.len(), None);

    unsafe {
      let (dst_sample_format, dst_sample_rate) = 
        (self.dst_sample_format, self.dst_sample_rate);

      self.dst_frame_pool = FramePool::new(FramePoolParams::Audio {
        layout: self.layout,
        format: dst_sample_format,
        samplerate: dst_sample_rate
      });

      let ret = ffi::swr_alloc_set_opts2(
        &mut self.multi_sampler,
        &self.layout,
        dst_sample_format,
        dst_sample_rate,
        &self.layout,
        src_sample_format,
        src_sample_rate,
        0,
        std::ptr::null_mut(),
      );
      assert!(ret >= 0, "swr_alloc_set_opts2: {}", av_error_string(ret));
      let ret = ffi::swr_init(self.multi_sampler);
      assert!(ret >= 0, "swr_init: {}", av_error_string(ret));

      self.single_samplers =self.input_channels.range.clone().map(|_| {
        let mut sampler = std::ptr::null_mut();
        let ret = ffi::swr_alloc_set_opts2(
          &mut sampler,
          &self.single_layout,
          dst_sample_format,
          dst_sample_rate,
          &self.single_layout,
          src_sample_format,
          src_sample_rate,
          0,
          std::ptr::null_mut(),
        );
        assert!(ret >= 0, "swr_alloc_set_opts2: {}", av_error_string(ret));
        let ret = ffi::swr_init(sampler);
        assert!(ret >= 0, "swr_init: {}", av_error_string(ret));
        sampler
      }).collect();

      // The reducer joins the single-channel samplers' outputs, which are
      // already in the destination format and rate, one plane per channel.
      let reducer_sample_format = ffi::av_get_planar_sample_fmt(dst_sample_format);
      let ret = ffi::swr_alloc_set_opts2(
        &mut self.reducer_sampler,
        &self.layout,
        dst_sample_format,
        dst_sample_rate,
        &self.layout,
        reducer_sample_format,
        dst_sample_rate,
        0,
        std::ptr::null_mut(),
      );
      assert!(ret >= 0, "swr_alloc_set_opts2: {}", av_error_string(ret));
      let ret = ffi::swr_init(self.reducer_sampler);
      assert!(ret >= 0, "swr_init: {}", av_error_string(ret));

      let needs_decoding = self.decoder.is_some();
      let needs_encoding = self.encoder.is_some();
      let needs_sampling = dst_sample_rate != src_sample_rate || dst_sample_format != src_sample_format;
      self.needs_conversion = needs_decoding || needs_encoding || needs_sampling;
    }
  }

  fn get_input_range(&self, input: &dyn AnalogData) -> InputChannels {
    let num_channels = input.num_channels();
    let mut format: Option<(AnalogFormat, Duration)> = None;
    let encoding = input.encoding();
    for i in (0..num_channels).rev() {
      let next_format = input.analog_format(i);
      let next_samplerate = input.sample_interval(i);
      let next = (next_format, next_samplerate);
      match format {
        Some(f) => {
          if f != next {
            let range = (i+1)..num_channels;
            let names = range.clone().map(|i| input.name(i).to_string()).collect();
            return InputChannels {
              range,
              names,
              format: f.0,
              sample_interval: f.1,
              encoding,
            };
          }
        }
        None => {
          format = Some(next);
        }
      }
    }
    let range = 0..num_channels;
    let names = range.clone().map(|i| input.name(i).to_string()).collect();
    let result_format = format.unwrap_or((AnalogFormat::Double, Duration::default()));
    InputChannels { range, format: result_format.0, sample_interval: result_format.1, names, encoding }
  }

  fn convert_samples(&mut self, multi_sampler: *mut ffi::SwrContext, in_ptrs: &[*const u8], in_counts: &[i32], time: Duration, recursing: bool) {
    unsafe {
      if in_counts.iter().sum::<i32>() == 0 {
        return;
      }
      let first_count = in_counts[0];
      let use_multi = 'check_multi: {
        if recursing {
          break 'check_multi true;
        }
        let sizes_equal = self.input_channels.range.clone().enumerate().all(|(i, _)| first_count == in_counts[i]);
        let buffers_empty = self.sample_buffers.iter().all(|b|b.is_empty());
        sizes_equal && buffers_empty
      };

      if use_multi {
        let pts = self.pts;
        self.pts_to_time.push_back((pts, time));
        self.pts += 1;

        let in_samples = first_count;
        let out_samples = ffi::swr_get_out_samples(multi_sampler, in_samples as i32);

        let frame = self.dst_frame_pool.get_writable(out_samples as i32);
        
        let converted = ffi::swr_convert(multi_sampler, (*frame).extended_data, out_samples, in_ptrs.as_ptr(), in_samples as i32);
        assert!(converted >= 0, "swr_convert: {}", av_error_string(converted));
        (*frame).nb_samples = converted;
        (*frame).pts = pts;
        self.dst_frame_pool.push_pending(frame);
      } else {
        // Buffers hold output-rate samples in the destination format; all
        // counts below are in samples and converted to bytes only to index.
        let out_bps = ffi::av_get_bytes_per_sample(self.dst_sample_format) as usize;
        let out_rate = self.dst_sample_rate;
        for ui in 0..self.input_channels.range.len() {
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
            time.saturating_sub(self.input_channels.sample_interval * (in_samples - 1) as u32)
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
          self.convert_samples(self.reducer_sampler, &ptrs, &counts, time, true);
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

  pub fn needs_conversion(&self) -> bool {
    self.needs_conversion
  }

  pub fn reconfigure(&mut self, params: AudioConverterParams) {
    self.params = params;
    self.cleanup();
  }

  /// Queues `data`'s analog data for conversion, if it needs it.
  pub fn push(&mut self, data: &dyn NodeData) {
    let Some(input) = data.analog() else {
      return;
    };

    if !self.decoder_configured {
      let input_channels = self.get_input_range(input);
      if input_channels.sample_interval.is_zero() || !is_supported_input(&input_channels) {
        if !self.rejection_logged {
          println!(
            "AudioConverter can't convert {:?} ({:?}) input with a {:?} sample interval",
            input_channels.format, input_channels.encoding, input_channels.sample_interval);
        }
        self.rejection_logged = true;
        return;
      }
      self.configure_decoder(input_channels);
    }

    if let Some(_) = self.decoder {
      let pts = self.pts;
      self.pts_to_time.push_back((pts, data.time()));
      self.pts += 1;

      let plane = input.buffer();

      let buffer_pos = self.in_buffer.len() - AV_INPUT_BUFFER_PADDING_SIZE as usize;
      self.in_buffer.resize(self.in_buffer.len() + plane.len(), 0);

      let end = buffer_pos+plane.len();
      self.in_buffer[buffer_pos..end].copy_from_slice(plane);
      self.slice_to_pts.push_back((end, pts));
    } else {
      let src_sample_format = analog_to_sample_format(self.input_channels.format);
      let src_sample_rate = interval_to_rate(self.input_channels.sample_interval, self.params.samplerate);
      self.configure_encoder(src_sample_format, src_sample_rate);
      self.configure_resampler(src_sample_format, src_sample_rate);

      let pointers: Vec<_> = self.input_channels.range.clone().map(|i| analog_data_ptr(input, i)).collect();
      let counts: Vec<_> = self.input_channels.range.clone().map(|i| input.count(i) as i32).collect();
      self.convert_samples(self.multi_sampler, pointers.as_slice(), counts.as_slice(), data.time(), false);
    }
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

  /// Parses buffered input until one packet is sent to the decoder. Returns
  /// false when no complete packet is buffered. Only called after
  /// avcodec_receive_frame returned EAGAIN, so the decoder accepts the packet.
  fn send_next_packet(&mut self, decoder: *mut ffi::AVCodecContext) -> bool {
    let padding = AV_INPUT_BUFFER_PADDING_SIZE as usize;
    unsafe {
      loop {
        let available = self.in_buffer.len().saturating_sub(padding);
        if available == 0 {
          return false;
        }
        let pts = self.slice_to_pts.front().map_or(ffi::AV_NOPTS_VALUE, |(_, pts)| *pts);

        let used = ffi::av_parser_parse2(
          self.parser, decoder,
          &mut (*self.parser_packet).data,
          &mut (*self.parser_packet).size,
          self.in_buffer.as_ptr(),
          available as i32,
          pts, ffi::AV_NOPTS_VALUE,
          self.num_input_bytes
        );
        assert!(used >= 0, "av_parser_parse2: {}", av_error_string(used));
        self.consume_input(used as usize);

        if (*self.parser_packet).size > 0 {
          (*self.parser_packet).pts = (*self.parser).pts;
          let ret = ffi::avcodec_send_packet(decoder, self.parser_packet);
          assert!(ret >= 0, "avcodec_send_packet {}", av_error_string(ret));
          return true;
        }
        if used == 0 {
          return false;
        }
      }
    }
  }

  /// The next frame to output or encode: a decoded (and, if needed,
  /// resampled) frame for encoded input, or a converted frame for raw input.
  fn next_frame(&mut self) -> Option<*mut ffi::AVFrame> {
    let Some(decoder) = self.decoder else {
      return self.dst_frame_pool.get_pending();
    };
    unsafe {
      loop {
        let ret = ffi::avcodec_receive_frame(decoder, self.decoder_frame);
        if ret == AVERROR_EAGAIN {
          if !self.send_next_packet(decoder) {
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

      let src_sample_format = std::mem::transmute::<i32, ffi::AVSampleFormat>((*self.decoder_frame).format);
      let src_sample_rate = (*self.decoder_frame).sample_rate;
      let nb_samples = (*self.decoder_frame).nb_samples;
      self.configure_encoder(src_sample_format, src_sample_rate);
      self.configure_resampler(src_sample_format, src_sample_rate);

      // Already in the output format: hand out a new reference to the
      // decoded buffers instead of converting. decoder_frame is reused by the
      // next avcodec_receive_frame, so it can't be handed out itself.
      let same_layout = ffi::av_channel_layout_compare(&(*self.decoder_frame).ch_layout, &self.layout) == 0;
      if same_layout && src_sample_format == self.dst_sample_format && src_sample_rate == self.dst_sample_rate {
        let frame = ffi::av_frame_clone(self.decoder_frame);
        assert!(!frame.is_null(), "av_frame_clone failed");
        ffi::av_frame_unref(self.decoder_frame);
        return Some(frame);
      }

      let out_samples = ffi::swr_get_out_samples(self.multi_sampler, nb_samples);
      assert!(out_samples >= 0, "swr_get_out_samples: {}", av_error_string(out_samples));
      let frame = self.dst_frame_pool.get_writable(out_samples);
      let converted = ffi::swr_convert(
        self.multi_sampler,
        (*frame).extended_data,
        out_samples,
        (*self.decoder_frame).extended_data as *const *const u8,
        nb_samples);
      assert!(converted >= 0, "swr_convert: {}", av_error_string(converted));
      (*frame).nb_samples = converted;
      (*frame).pts = (*self.decoder_frame).pts;
      ffi::av_frame_unref(self.decoder_frame);
      self.dst_frame_pool.push_pending(frame);
      self.dst_frame_pool.get_pending()
    }
  }

  /// The time of the input message that produced `pts`. Earlier entries
  /// belong to messages that produced no frame of their own and are dropped.
  fn take_time(&mut self, pts: i64) -> Duration {
    match self.pts_to_time.iter().position(|(p, _)| *p == pts) {
      Some(i) => {
        let time = self.pts_to_time[i].1;
        self.pts_to_time.drain(..=i);
        time
      }
      None => self.api.time(),
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

  /// One output per frame. Encoded outputs are emitted even when the encoder
  /// produced no packets yet: the buffer is empty and encoded_count is the
  /// number of samples pushed.
  pub fn pull(&mut self) -> Option<AudioOutput<'_>> {
    let frame = self.next_frame()?;
    let time = self.take_time(unsafe { (*frame).pts });
    let Some(encoder) = self.encoder else {
      return Some(AudioOutput { converter: self, frame: Some(frame), time, encoded_count: 0 });
    };
    self.out_buffer.clear();
    let encoded_count = self.encode_frame(encoder, frame);
    Some(AudioOutput { converter: self, frame: None, time, encoded_count })
  }
}

impl Drop for AudioConverter {
  fn drop(&mut self) {
    self.cleanup();
  }
}
#[cfg(test)]
mod tests {
  use super::*;

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
}
