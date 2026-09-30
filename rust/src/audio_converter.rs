//! The audio half of the converter: turns analog data into a target audio
//! format. Raw formats are converted sample by sample; AAC is encoded with
//! FFmpeg (swresample -> audio FIFO -> aac encoder, framed as ADTS) and
//! decoded with FFmpeg (aac parser -> aac decoder).
//!
//! Integer and float samples are related the way audio code usually relates
//! them: an i16 of 32767 is just under 1.0, an i32 of 2^31 - 1 is just under
//! 1.0.

use std::collections::VecDeque;
use std::time::Duration;

use ffmpeg_sys_next as ffi;

use crate::api::{AnalogData, AnalogEncoding, AnalogFormat, NodeData};
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

/// Sample rates AAC can carry, in ADTS sampling_frequency_index order.
const AAC_SAMPLE_RATES: [u32; 13] = [
  96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// Channel counts FFmpeg's AAC encoder supports with a standard ADTS
/// channel_configuration (other layouts need an in-band program config
/// element many decoders don't handle).
const AAC_CHANNEL_COUNTS: [usize; 7] = [1, 2, 3, 4, 5, 6, 8];

/// AAC-LC bitrate per channel when AudioConverterParams::bitrate isn't set.
const AAC_BITRATE_PER_CHANNEL: i64 = 64_000;

/// Input rates within this fraction of an AAC rate are treated as that rate
/// rather than resampled (sample intervals are whole nanoseconds, so e.g.
/// 48 kHz arrives as 20833 ns, about 48001.9 Hz).
const RATE_TOLERANCE: f64 = 0.001;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AudioConverterParams {
  /// None passes analog data through unconverted.
  pub format: Option<AudioFormat>,
  /// Bitrate of encoded formats in bits per second, for the whole stream;
  /// None means 64 kbit/s per channel.
  pub bitrate: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AudioFormat {
  /// i16 samples.
  Integer,
  /// f64 samples.
  Decimal,
  /// ADTS-framed AAC-LC in the analog buffer.
  AAC,
}

/// The sample representation of an analog message.
#[derive(Debug, Clone, Copy, PartialEq)]
enum InputKind {
  Short,
  Int,
  ULong,
  Double,
  AAC,
}

fn input_kind(analog: &dyn AnalogData) -> InputKind {
  if analog.encoding() == AnalogEncoding::AAC {
    InputKind::AAC
  } else if analog.is_short_data() {
    InputKind::Short
  } else if analog.is_int_data() {
    InputKind::Int
  } else if analog.is_ulong_data() {
    InputKind::ULong
  } else {
    InputKind::Double
  }
}

/// Whether analog data of `kind` must be converted to be in `target`. u64
/// data (counters, not audio) is never converted.
fn kind_needs_conversion(kind: InputKind, target: AudioFormat) -> bool {
  match (kind, target) {
    (InputKind::ULong, _) => false,
    (InputKind::Short, AudioFormat::Integer) => false,
    (InputKind::Double, AudioFormat::Decimal) => false,
    (InputKind::AAC, AudioFormat::AAC) => false,
    _ => true,
  }
}

enum InputSamples {
  Short(Vec<Vec<i16>>),
  Int(Vec<Vec<i32>>),
  Double(Vec<Vec<f64>>),
  AAC(Vec<u8>),
}

/// An analog message copied out of its NodeData, waiting to be converted.
struct AudioInput {
  time: Duration,
  names: Vec<String>,
  intervals: Vec<Duration>,
  samples: InputSamples,
}

fn read_input(time: Duration, analog: &dyn AnalogData) -> Option<AudioInput> {
  let channels = 0..analog.num_channels().max(0);
  let names = channels.clone().map(|c| analog.name(c).to_string()).collect();
  let intervals = channels.clone().map(|c| analog.sample_interval(c)).collect();
  let samples = match input_kind(analog) {
    InputKind::AAC => InputSamples::AAC(analog.buffer().to_vec()),
    InputKind::Short => InputSamples::Short(channels.map(|c| analog.short_data(c).to_vec()).collect()),
    InputKind::Int => InputSamples::Int(channels.map(|c| analog.int_data(c).to_vec()).collect()),
    InputKind::Double => InputSamples::Double(channels.map(|c| analog.data(c).to_vec()).collect()),
    InputKind::ULong => return None,
  };
  Some(AudioInput {
    time,
    names,
    intervals,
    samples,
  })
}

fn i16_to_f64(s: i16) -> f64 {
  f64::from(s) / 32768.0
}

fn i32_to_f64(s: i32) -> f64 {
  f64::from(s) / 2_147_483_648.0
}

fn f64_to_i16(s: f64) -> i16 {
  (s * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

fn i32_to_i16(s: i32) -> i16 {
  (s >> 16) as i16
}

fn raw_to_f64(samples: &InputSamples) -> Vec<Vec<f64>> {
  match samples {
    InputSamples::Short(c) => c.iter().map(|c| c.iter().map(|&s| i16_to_f64(s)).collect()).collect(),
    InputSamples::Int(c) => c.iter().map(|c| c.iter().map(|&s| i32_to_f64(s)).collect()).collect(),
    InputSamples::Double(c) => c.clone(),
    InputSamples::AAC(_) => Vec::new(),
  }
}

fn raw_to_i16(samples: &InputSamples) -> Vec<Vec<i16>> {
  match samples {
    InputSamples::Short(c) => c.clone(),
    InputSamples::Int(c) => c.iter().map(|c| c.iter().map(|&s| i32_to_i16(s)).collect()).collect(),
    InputSamples::Double(c) => c.iter().map(|c| c.iter().map(|&s| f64_to_i16(s)).collect()).collect(),
    InputSamples::AAC(_) => Vec::new(),
  }
}

/// The largest channel count the AAC encoder supports that isn't more than
/// `channels`; extra channels are dropped.
fn aac_channel_count(channels: usize) -> Option<usize> {
  AAC_CHANNEL_COUNTS.iter().rev().copied().find(|&c| c <= channels)
}

/// ADTS channel_configuration for a supported channel count.
fn adts_channel_config(channels: usize) -> u8 {
  if channels == 8 { 7 } else { channels as u8 }
}

/// (input rate, AAC rate, ADTS sampling_frequency_index) for audio sampled
/// every `interval`. The input rate is snapped to the AAC rate when they're
/// within RATE_TOLERANCE, so no resampling happens.
fn aac_rates(interval: Duration) -> Option<(u32, u32, u8)> {
  if interval.is_zero() {
    return None;
  }
  let rate = 1.0 / interval.as_secs_f64();
  let (index, &aac_rate) = AAC_SAMPLE_RATES
    .iter()
    .enumerate()
    .min_by(|(_, a), (_, b)| (f64::from(**a) - rate).abs().total_cmp(&(f64::from(**b) - rate).abs()))?;
  let input_rate = if (f64::from(aac_rate) - rate).abs() <= f64::from(aac_rate) * RATE_TOLERANCE {
    aac_rate
  } else {
    rate.round() as u32
  };
  Some((input_rate, aac_rate, index as u8))
}

/// A 7-byte ADTS header (no CRC) for an AAC-LC frame of `payload_len` bytes.
fn adts_header(payload_len: usize, rate_index: u8, channel_config: u8) -> [u8; 7] {
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

fn sample_interval(rate: u32) -> Duration {
  Duration::from_secs_f64(1.0 / f64::from(rate))
}

/// FFmpeg's AAC encoder plus what feeds it: a resampler converting f64
/// planes to its float-planar input (and to an AAC sample rate if needed)
/// and a FIFO collecting samples into its fixed-size frames.
struct AacEncoder {
  context: *mut ffi::AVCodecContext,
  resampler: *mut ffi::SwrContext,
  fifo: *mut ffi::AVAudioFifo,
  frame: *mut ffi::AVFrame,
  packet: *mut ffi::AVPacket,
  channels: usize,
  input_rate: u32,
  rate_index: u8,
  bitrate: i64,
  next_pts: i64,
}

impl AacEncoder {
  fn new(channels: usize, input_rate: u32, aac_rate: u32, rate_index: u8, bitrate: i64) -> Result<AacEncoder, String> {
    unsafe {
      let codec = ffi::avcodec_find_encoder(ffi::AVCodecID::AV_CODEC_ID_AAC);
      if codec.is_null() {
        return Err("no AAC encoder in this FFmpeg build".to_string());
      }
      let mut encoder = AacEncoder {
        context: ffi::avcodec_alloc_context3(codec),
        resampler: std::ptr::null_mut(),
        fifo: std::ptr::null_mut(),
        frame: ffi::av_frame_alloc(),
        packet: ffi::av_packet_alloc(),
        channels,
        input_rate,
        rate_index,
        bitrate,
        next_pts: 0,
      };
      let context = encoder.context;
      if context.is_null() || encoder.frame.is_null() || encoder.packet.is_null() {
        return Err("FFmpeg allocation failed".to_string());
      }
      let mut layout: ffi::AVChannelLayout = std::mem::zeroed();
      ffi::av_channel_layout_default(&mut layout, channels as i32);
      (*context).sample_fmt = ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP;
      (*context).sample_rate = aac_rate as i32;
      (*context).time_base = ffi::AVRational { num: 1, den: aac_rate as i32 };
      (*context).bit_rate = bitrate;
      ffi::av_channel_layout_copy(&mut (*context).ch_layout, &layout);
      let ret = ffi::avcodec_open2(context, codec, std::ptr::null_mut());
      if ret < 0 {
        ffi::av_channel_layout_uninit(&mut layout);
        return Err(format!("opening AAC encoder failed: {}", av_error_string(ret)));
      }

      let ret = ffi::swr_alloc_set_opts2(
        &mut encoder.resampler,
        &layout,
        ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP,
        aac_rate as i32,
        &layout,
        ffi::AVSampleFormat::AV_SAMPLE_FMT_DBLP,
        input_rate as i32,
        0,
        std::ptr::null_mut(),
      );
      if ret < 0 || ffi::swr_init(encoder.resampler) < 0 {
        ffi::av_channel_layout_uninit(&mut layout);
        return Err(format!("setting up resampler failed: {}", av_error_string(ret)));
      }

      encoder.fifo = ffi::av_audio_fifo_alloc(ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP, channels as i32, 1);
      (*encoder.frame).nb_samples = (*context).frame_size;
      (*encoder.frame).format = ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP as i32;
      (*encoder.frame).sample_rate = aac_rate as i32;
      ffi::av_channel_layout_copy(&mut (*encoder.frame).ch_layout, &layout);
      ffi::av_channel_layout_uninit(&mut layout);
      let ret = ffi::av_frame_get_buffer(encoder.frame, 0);
      if encoder.fifo.is_null() || ret < 0 {
        return Err("allocating audio buffers failed".to_string());
      }
      Ok(encoder)
    }
  }

  /// Encodes `planes` (one per channel, equal lengths, -1..1) and returns
  /// the ADTS frames that came out, and how many samples per channel went
  /// into the encoder (after resampling). The encoder works in fixed-size
  /// frames, so leftover samples come out of a later call.
  fn encode(&mut self, planes: &[Vec<f64>]) -> (Vec<u8>, u64) {
    let mut output = Vec::new();
    let frames = planes.first().map_or(0, |p| p.len());
    unsafe {
      let capacity = ffi::swr_get_out_samples(self.resampler, frames as i32).max(0) as usize;
      let mut resampled: Vec<Vec<f32>> = vec![vec![0.0; capacity]; self.channels];
      let out_ptrs: Vec<*mut u8> = resampled.iter_mut().map(|p| p.as_mut_ptr() as *mut u8).collect();
      let in_ptrs: Vec<*const u8> = planes.iter().map(|p| p.as_ptr() as *const u8).collect();
      let converted = ffi::swr_convert(
        self.resampler,
        out_ptrs.as_ptr(),
        capacity as i32,
        in_ptrs.as_ptr(),
        frames as i32,
      );
      assert!(converted >= 0, "swr_convert: {}", av_error_string(converted));
      let fifo_ptrs: Vec<*mut std::ffi::c_void> = out_ptrs.iter().map(|&p| p as *mut std::ffi::c_void).collect();
      let written = ffi::av_audio_fifo_write(self.fifo, fifo_ptrs.as_ptr(), converted);
      assert!(written >= 0, "av_audio_fifo_write: {}", av_error_string(written));

      let frame_size = (*self.context).frame_size;
      while ffi::av_audio_fifo_size(self.fifo) >= frame_size {
        let ret = ffi::av_frame_make_writable(self.frame);
        assert!(ret >= 0, "av_frame_make_writable: {}", av_error_string(ret));
        let read = ffi::av_audio_fifo_read(
          self.fifo,
          (*self.frame).data.as_ptr() as *const *mut std::ffi::c_void,
          frame_size,
        );
        assert!(read == frame_size, "av_audio_fifo_read: {}", av_error_string(read));
        (*self.frame).pts = self.next_pts;
        self.next_pts += i64::from(frame_size);
        let ret = ffi::avcodec_send_frame(self.context, self.frame);
        assert!(ret >= 0, "avcodec_send_frame: {}", av_error_string(ret));
        self.receive_packets(&mut output);
      }
      (output, converted as u64)
    }
  }

  fn receive_packets(&mut self, output: &mut Vec<u8>) {
    let channel_config = adts_channel_config(self.channels);
    unsafe {
      loop {
        let ret = ffi::avcodec_receive_packet(self.context, self.packet);
        if ret == AVERROR_EAGAIN || ret == AVERROR_EOF {
          break;
        }
        assert!(ret >= 0, "avcodec_receive_packet: {}", av_error_string(ret));
        let payload = std::slice::from_raw_parts((*self.packet).data, (*self.packet).size as usize);
        output.extend_from_slice(&adts_header(payload.len(), self.rate_index, channel_config));
        output.extend_from_slice(payload);
        ffi::av_packet_unref(self.packet);
      }
    }
  }
}

impl Drop for AacEncoder {
  fn drop(&mut self) {
    unsafe {
      ffi::avcodec_free_context(&mut self.context);
      ffi::swr_free(&mut self.resampler);
      if !self.fifo.is_null() {
        ffi::av_audio_fifo_free(self.fifo);
      }
      ffi::av_frame_free(&mut self.frame);
      ffi::av_packet_free(&mut self.packet);
    }
  }
}

/// FFmpeg's AAC parser and decoder, reading ADTS.
struct AacDecoder {
  context: *mut ffi::AVCodecContext,
  parser: *mut ffi::AVCodecParserContext,
  frame: *mut ffi::AVFrame,
  packet: *mut ffi::AVPacket,
  input: Vec<u8>,
}

impl AacDecoder {
  fn new() -> Result<AacDecoder, String> {
    unsafe {
      let codec = ffi::avcodec_find_decoder(ffi::AVCodecID::AV_CODEC_ID_AAC);
      if codec.is_null() {
        return Err("no AAC decoder in this FFmpeg build".to_string());
      }
      let decoder = AacDecoder {
        context: ffi::avcodec_alloc_context3(codec),
        parser: ffi::av_parser_init(ffi::AVCodecID::AV_CODEC_ID_AAC as i32),
        frame: ffi::av_frame_alloc(),
        packet: ffi::av_packet_alloc(),
        input: Vec::new(),
      };
      if decoder.context.is_null() || decoder.parser.is_null() || decoder.frame.is_null() || decoder.packet.is_null() {
        return Err("FFmpeg allocation failed".to_string());
      }
      let ret = ffi::avcodec_open2(decoder.context, codec, std::ptr::null_mut());
      if ret < 0 {
        return Err(format!("opening AAC decoder failed: {}", av_error_string(ret)));
      }
      Ok(decoder)
    }
  }

  /// Decodes as much of `bytes` as forms whole frames (the parser holds back
  /// the last frame until the next one starts) and returns the samples, one
  /// Vec per channel, with their sample rate.
  fn decode(&mut self, bytes: &[u8]) -> (Vec<Vec<f32>>, u32) {
    let mut channels: Vec<Vec<f32>> = Vec::new();
    let mut rate = 0;
    let padding = ffi::AV_INPUT_BUFFER_PADDING_SIZE as usize;
    self.input.clear();
    self.input.extend_from_slice(bytes);
    self.input.resize(bytes.len() + padding, 0);
    let mut offset = 0;
    unsafe {
      // Never passes the parser an empty buffer: that means end of stream,
      // and would flush a frame the next message completes.
      while offset < bytes.len() {
        let used = ffi::av_parser_parse2(
          self.parser,
          self.context,
          &mut (*self.packet).data,
          &mut (*self.packet).size,
          self.input.as_ptr().add(offset),
          (bytes.len() - offset) as i32,
          ffi::AV_NOPTS_VALUE,
          ffi::AV_NOPTS_VALUE,
          0,
        );
        assert!(used >= 0, "av_parser_parse2: {}", av_error_string(used));
        offset += used as usize;
        if (*self.packet).size > 0 {
          let ret = ffi::avcodec_send_packet(self.context, self.packet);
          if ret < 0 {
            println!("AAC decoder rejected a packet: {}", av_error_string(ret));
          }
          self.receive_frames(&mut channels, &mut rate);
        }
        if used == 0 && (*self.packet).size == 0 {
          break;
        }
      }
    }
    (channels, rate)
  }

  fn receive_frames(&mut self, channels: &mut Vec<Vec<f32>>, rate: &mut u32) {
    unsafe {
      loop {
        let ret = ffi::avcodec_receive_frame(self.context, self.frame);
        if ret == AVERROR_EAGAIN || ret == AVERROR_EOF {
          break;
        }
        assert!(ret >= 0, "avcodec_receive_frame: {}", av_error_string(ret));
        // FFmpeg's AAC decoder always produces float planar audio.
        if (*self.frame).format != ffi::AVSampleFormat::AV_SAMPLE_FMT_FLTP as i32 {
          println!("AAC decoder produced unexpected sample format {}", (*self.frame).format);
          ffi::av_frame_unref(self.frame);
          continue;
        }
        let count = (*self.frame).ch_layout.nb_channels as usize;
        let samples = (*self.frame).nb_samples as usize;
        if channels.len() != count {
          channels.resize(count, Vec::new());
        }
        for (c, channel) in channels.iter_mut().enumerate() {
          let plane = *(*self.frame).extended_data.add(c) as *const f32;
          channel.extend_from_slice(std::slice::from_raw_parts(plane, samples));
        }
        *rate = (*self.frame).sample_rate as u32;
        ffi::av_frame_unref(self.frame);
      }
    }
  }
}

impl Drop for AacDecoder {
  fn drop(&mut self) {
    unsafe {
      ffi::avcodec_free_context(&mut self.context);
      if !self.parser.is_null() {
        ffi::av_parser_close(self.parser);
      }
      ffi::av_frame_free(&mut self.frame);
      ffi::av_packet_free(&mut self.packet);
    }
  }
}

enum OutputSamples {
  Short(Vec<Vec<i16>>),
  Double(Vec<Vec<f64>>),
  /// `samples` per channel went into the encoder for this output; `buffer`
  /// holds whatever frames the encoder has produced so far, possibly none.
  AAC { buffer: Vec<u8>, channels: usize, samples: u64 },
}

/// One converted analog message.
pub struct AudioOutput {
  time: Duration,
  names: Vec<String>,
  intervals: Vec<Duration>,
  samples: OutputSamples,
}

impl NodeData for AudioOutput {
  fn time(&self) -> Duration {
    self.time
  }

  fn analog(&self) -> Option<&dyn AnalogData> {
    Some(self)
  }
}

fn channel_slice<T>(channels: &[Vec<T>], channel: i32) -> &[T] {
  usize::try_from(channel)
    .ok()
    .and_then(|i| channels.get(i))
    .map_or(&[], |c| c.as_slice())
}

impl AnalogData for AudioOutput {
  fn data(&self, channel: i32) -> &[f64] {
    match &self.samples {
      OutputSamples::Double(c) => channel_slice(c, channel),
      _ => &[],
    }
  }

  fn short_data(&self, channel: i32) -> &[i16] {
    match &self.samples {
      OutputSamples::Short(c) => channel_slice(c, channel),
      _ => &[],
    }
  }

  fn is_short_data(&self) -> bool {
    matches!(self.samples, OutputSamples::Short(_))
  }

  /// AAC output reports its channels (names and sample intervals) with no
  /// samples; the audio is in the buffer.
  fn num_channels(&self) -> i32 {
    match &self.samples {
      OutputSamples::Short(c) => c.len() as i32,
      OutputSamples::Double(c) => c.len() as i32,
      OutputSamples::AAC { channels, .. } => *channels as i32,
    }
  }

  fn sample_interval(&self, channel: i32) -> Duration {
    usize::try_from(channel)
      .ok()
      .and_then(|i| self.intervals.get(i).copied())
      .unwrap_or_default()
  }

  fn name(&self, channel: i32) -> &str {
    usize::try_from(channel)
      .ok()
      .and_then(|i| self.names.get(i))
      .map_or("", |n| n.as_str())
  }

  fn buffer(&self) -> &[u8] {
    match &self.samples {
      OutputSamples::AAC { buffer, .. } => buffer,
      _ => &[],
    }
  }

  fn encoding(&self) -> AnalogEncoding {
    match self.samples {
      OutputSamples::AAC { .. } => AnalogEncoding::AAC,
      _ => AnalogEncoding::None,
    }
  }

  fn analog_format(&self, _channel: i32) -> AnalogFormat {
    match self.samples {
      OutputSamples::Short(_) => AnalogFormat::Short,
      OutputSamples::Double(_) => AnalogFormat::Double,
      OutputSamples::AAC { .. } => AnalogFormat::Encoded,
    }
  }

  fn encoded_count(&self) -> u64 {
    match self.samples {
      OutputSamples::AAC { samples, .. } => samples,
      _ => 0,
    }
  }
}

/// Converts analog data to a target audio format. Like the image Converter,
/// push() only copies the input; the conversion happens in pull(), so it
/// runs wherever pull() is called.
pub struct AudioConverter {
  params: AudioConverterParams,
  inputs: VecDeque<AudioInput>,
  encoder: Option<AacEncoder>,
  decoder: Option<AacDecoder>,
}

// SAFETY: the FFmpeg contexts are only used through &mut self, and FFmpeg
// codec contexts may move between threads as long as they aren't used
// concurrently.
unsafe impl Send for AudioConverter {}

impl AudioConverter {
  pub fn new(params: AudioConverterParams) -> AudioConverter {
    AudioConverter {
      params,
      inputs: VecDeque::new(),
      encoder: None,
      decoder: None,
    }
  }

  /// Changes the parameters, dropping queued input and codec state. Does
  /// nothing if they're unchanged.
  pub fn reconfigure(&mut self, params: AudioConverterParams) {
    if params == self.params {
      return;
    }
    self.params = params;
    self.inputs.clear();
    self.encoder = None;
    self.decoder = None;
  }

  pub fn needs_conversion(&self, data: &dyn NodeData) -> bool {
    let (Some(target), Some(analog)) = (self.params.format, data.analog()) else {
      return false;
    };
    if analog.num_channels() <= 0 && analog.encoding() == AnalogEncoding::None {
      return false;
    }
    kind_needs_conversion(input_kind(analog), target)
  }

  /// Queues `data`'s analog data for conversion, if it needs it.
  pub fn push(&mut self, data: &dyn NodeData) {
    if !self.needs_conversion(data) {
      return;
    }
    if let Some(input) = data.analog().and_then(|analog| read_input(data.time(), analog)) {
      self.inputs.push_back(input);
    }
  }

  /// Converts queued input until something comes out. Every input encoded
  /// to AAC produces an output (with encoded_count samples, even if the
  /// encoder hasn't output a frame yet); other inputs that produce nothing,
  /// e.g. AAC too short to decode a frame from, are consumed without one.
  pub fn pull(&mut self) -> Option<AudioOutput> {
    while let Some(input) = self.inputs.pop_front() {
      if let Some(output) = self.convert(input) {
        return Some(output);
      }
    }
    None
  }

  fn convert(&mut self, input: AudioInput) -> Option<AudioOutput> {
    let target = self.params.format?;
    match (&input.samples, target) {
      (InputSamples::AAC(bytes), AudioFormat::Integer | AudioFormat::Decimal) => {
        self.decode(input.time, bytes, &input.names, target)
      }
      (InputSamples::AAC(_), AudioFormat::AAC) => None,
      (_, AudioFormat::Integer) => Some(AudioOutput {
        samples: OutputSamples::Short(raw_to_i16(&input.samples)),
        time: input.time,
        names: input.names,
        intervals: input.intervals,
      }),
      (_, AudioFormat::Decimal) => Some(AudioOutput {
        samples: OutputSamples::Double(raw_to_f64(&input.samples)),
        time: input.time,
        names: input.names,
        intervals: input.intervals,
      }),
      (_, AudioFormat::AAC) => self.encode(input),
    }
  }

  fn encode(&mut self, input: AudioInput) -> Option<AudioOutput> {
    let interval = *input.intervals.first()?;
    // AAC needs one sample rate, so only channels sampled like the first
    // are encoded, and only as many as AAC supports.
    let same_rate: Vec<usize> = (0..input.intervals.len())
      .filter(|&c| input.intervals[c] == interval)
      .collect();
    let channels = aac_channel_count(same_rate.len())?;
    let used = &same_rate[..channels];
    let (input_rate, aac_rate, rate_index) = aac_rates(interval)?;

    let all = raw_to_f64(&input.samples);
    let frames = used.iter().map(|&c| all[c].len()).min().unwrap_or(0);
    let planes: Vec<Vec<f64>> = used.iter().map(|&c| all[c][..frames].to_vec()).collect();

    let bitrate = self.params.bitrate.unwrap_or(AAC_BITRATE_PER_CHANNEL * channels as i64);
    let reusable = self.encoder.as_ref().is_some_and(|e| {
      e.channels == channels && e.input_rate == input_rate && e.rate_index == rate_index && e.bitrate == bitrate
    });
    if !reusable {
      self.encoder = match AacEncoder::new(channels, input_rate, aac_rate, rate_index, bitrate) {
        Ok(encoder) => Some(encoder),
        Err(e) => {
          println!("AudioConverter: {e}");
          return None;
        }
      };
    }
    let (buffer, samples) = self.encoder.as_mut()?.encode(&planes);
    Some(AudioOutput {
      time: input.time,
      names: used.iter().map(|&c| input.names[c].clone()).collect(),
      intervals: vec![sample_interval(aac_rate); channels],
      samples: OutputSamples::AAC { buffer, channels, samples },
    })
  }

  fn decode(&mut self, time: Duration, bytes: &[u8], names: &[String], target: AudioFormat) -> Option<AudioOutput> {
    if self.decoder.is_none() {
      self.decoder = match AacDecoder::new() {
        Ok(decoder) => Some(decoder),
        Err(e) => {
          println!("AudioConverter: {e}");
          return None;
        }
      };
    }
    let (channels, rate) = self.decoder.as_mut()?.decode(bytes);
    if channels.first().is_none_or(|c| c.is_empty()) || rate == 0 {
      return None;
    }
    let names = if names.len() == channels.len() {
      names.to_vec()
    } else {
      (0..channels.len()).map(|c| format!("Channel {c}")).collect()
    };
    let samples = match target {
      AudioFormat::Integer => OutputSamples::Short(
        channels.iter().map(|c| c.iter().map(|&s| f64_to_i16(f64::from(s))).collect()).collect(),
      ),
      _ => OutputSamples::Double(channels.iter().map(|c| c.iter().map(|&s| f64::from(s)).collect()).collect()),
    };
    Some(AudioOutput {
      time,
      intervals: vec![sample_interval(rate); channels.len()],
      names,
      samples,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Analog data for feeding the converter in tests.
  struct TestAnalog {
    time: Duration,
    interval: Duration,
    short: Option<Vec<Vec<i16>>>,
    double: Option<Vec<Vec<f64>>>,
    aac: Option<Vec<u8>>,
    names: Vec<String>,
  }

  impl TestAnalog {
    fn double(channels: Vec<Vec<f64>>, interval: Duration) -> TestAnalog {
      let names = (0..channels.len()).map(|c| format!("In {c}")).collect();
      TestAnalog { time: Duration::from_secs(1), interval, short: None, double: Some(channels), aac: None, names }
    }

    fn short(channels: Vec<Vec<i16>>, interval: Duration) -> TestAnalog {
      let names = (0..channels.len()).map(|c| format!("In {c}")).collect();
      TestAnalog { time: Duration::from_secs(1), interval, short: Some(channels), double: None, aac: None, names }
    }

    fn aac(buffer: Vec<u8>, channels: usize, interval: Duration) -> TestAnalog {
      let names = (0..channels).map(|c| format!("In {c}")).collect();
      TestAnalog { time: Duration::from_secs(1), interval, short: None, double: None, aac: Some(buffer), names }
    }
  }

  impl NodeData for TestAnalog {
    fn time(&self) -> Duration {
      self.time
    }
    fn analog(&self) -> Option<&dyn AnalogData> {
      Some(self)
    }
  }

  impl AnalogData for TestAnalog {
    fn data(&self, channel: i32) -> &[f64] {
      self.double.as_ref().map_or(&[], |c| channel_slice(c, channel))
    }
    fn short_data(&self, channel: i32) -> &[i16] {
      self.short.as_ref().map_or(&[], |c| channel_slice(c, channel))
    }
    fn is_short_data(&self) -> bool {
      self.short.is_some()
    }
    fn num_channels(&self) -> i32 {
      self.names.len() as i32
    }
    fn sample_interval(&self, _channel: i32) -> Duration {
      self.interval
    }
    fn name(&self, channel: i32) -> &str {
      &self.names[channel as usize]
    }
    fn buffer(&self) -> &[u8] {
      self.aac.as_deref().unwrap_or(&[])
    }
    fn encoding(&self) -> AnalogEncoding {
      if self.aac.is_some() { AnalogEncoding::AAC } else { AnalogEncoding::None }
    }
  }

  const INTERVAL_48K: Duration = Duration::from_nanos(20_833);

  fn params(format: AudioFormat) -> AudioConverterParams {
    AudioConverterParams { format: Some(format), bitrate: None }
  }

  fn sine(frequency: f64, rate: f64, samples: usize, phase: usize) -> Vec<f64> {
    (0..samples)
      .map(|i| 0.5 * (2.0 * std::f64::consts::PI * frequency * (i + phase) as f64 / rate).sin())
      .collect()
  }

  #[test]
  fn sample_conversions_use_audio_scaling() {
    assert_eq!(i16_to_f64(-32768), -1.0);
    assert_eq!(i16_to_f64(16384), 0.5);
    assert_eq!(i32_to_f64(i32::MIN), -1.0);
    assert_eq!(f64_to_i16(0.5), 16384);
    assert_eq!(f64_to_i16(-1.0), -32768);
    // Out of range values are clipped.
    assert_eq!(f64_to_i16(1.0), 32767);
    assert_eq!(f64_to_i16(-2.0), -32768);
    assert_eq!(i32_to_i16(i32::MAX), 32767);
    assert_eq!(i32_to_i16(i32::MIN), -32768);
  }

  #[test]
  fn only_mismatched_formats_need_conversion() {
    assert!(!kind_needs_conversion(InputKind::Short, AudioFormat::Integer));
    assert!(!kind_needs_conversion(InputKind::Double, AudioFormat::Decimal));
    assert!(!kind_needs_conversion(InputKind::AAC, AudioFormat::AAC));
    assert!(!kind_needs_conversion(InputKind::ULong, AudioFormat::Decimal));
    assert!(kind_needs_conversion(InputKind::Int, AudioFormat::Integer));
    assert!(kind_needs_conversion(InputKind::Short, AudioFormat::AAC));
    assert!(kind_needs_conversion(InputKind::AAC, AudioFormat::Decimal));
  }

  #[test]
  fn aac_channel_counts_drop_extra_channels() {
    assert_eq!(aac_channel_count(0), None);
    assert_eq!(aac_channel_count(2), Some(2));
    assert_eq!(aac_channel_count(7), Some(6));
    assert_eq!(aac_channel_count(8), Some(8));
    assert_eq!(aac_channel_count(20), Some(8));
    assert_eq!(adts_channel_config(8), 7);
    assert_eq!(adts_channel_config(6), 6);
  }

  #[test]
  fn aac_rates_snap_nearby_rates_and_resample_others() {
    // 20833 ns is 48 kHz after rounding to whole nanoseconds.
    assert_eq!(aac_rates(INTERVAL_48K), Some((48000, 48000, 3)));
    assert_eq!(aac_rates(Duration::from_nanos(22_676)), Some((44100, 44100, 4)));
    // 50 kHz isn't an AAC rate: resampled to the nearest one.
    assert_eq!(aac_rates(Duration::from_micros(20)), Some((50000, 48000, 3)));
    assert_eq!(aac_rates(Duration::ZERO), None);
  }

  #[test]
  fn adts_header_fields() {
    let header = adts_header(100, 3, 2);
    assert_eq!(header[0], 0xFF);
    assert_eq!(header[1] & 0xF6, 0xF0);
    // Profile AAC-LC, 48 kHz, stereo.
    assert_eq!(header[2] >> 6, 1);
    assert_eq!((header[2] >> 2) & 0xF, 3);
    let channel_config = ((header[2] & 1) << 2) | (header[3] >> 6);
    assert_eq!(channel_config, 2);
    let frame_len = ((usize::from(header[3]) & 3) << 11) | (usize::from(header[4]) << 3) | (usize::from(header[5]) >> 5);
    assert_eq!(frame_len, 107);
  }

  #[test]
  fn integer_and_decimal_conversions() {
    let mut converter = AudioConverter::new(params(AudioFormat::Decimal));
    let input = TestAnalog::short(vec![vec![16384, -32768]], INTERVAL_48K);
    assert!(converter.needs_conversion(&input));
    converter.push(&input);
    let output = converter.pull().unwrap();
    assert_eq!(output.data(0), &[0.5, -1.0]);
    assert_eq!(output.name(0), "In 0");
    assert_eq!(output.sample_interval(0), INTERVAL_48K);
    assert_eq!(output.time(), Duration::from_secs(1));

    converter.reconfigure(params(AudioFormat::Integer));
    let input = TestAnalog::double(vec![vec![0.5, 2.0]], INTERVAL_48K);
    converter.push(&input);
    let output = converter.pull().unwrap();
    assert!(output.is_short_data());
    assert_eq!(output.short_data(0), &[16384, 32767]);

    // Already in the target format.
    let input = TestAnalog::short(vec![vec![1]], INTERVAL_48K);
    assert!(!converter.needs_conversion(&input));
    converter.reconfigure(AudioConverterParams::default());
    assert!(!converter.needs_conversion(&input));
  }

  /// Encodes `channels` of sine waves to AAC in 480-sample chunks (like the
  /// MIC node's buffers) and returns the concatenated ADTS stream. Checks
  /// every chunk produces one output reporting its 480 samples.
  fn encode_sines(channels: usize, chunks: usize, bitrate: Option<i64>) -> (Vec<u8>, usize) {
    let mut converter = AudioConverter::new(AudioConverterParams { format: Some(AudioFormat::AAC), bitrate });
    let mut stream = Vec::new();
    let mut encoded_channels = 0;
    for chunk in 0..chunks {
      let planes = (0..channels).map(|c| sine(440.0 * (c + 1) as f64, 48000.0, 480, chunk * 480)).collect();
      converter.push(&TestAnalog::double(planes, INTERVAL_48K));
      let output = converter.pull().expect("every chunk should produce an output");
      assert!(converter.pull().is_none());
      assert_eq!(output.encoding(), AnalogEncoding::AAC);
      assert_eq!(output.encoded_count(), 480);
      assert_eq!(output.analog_format(0), AnalogFormat::Encoded);
      assert!(output.data(0).is_empty());
      assert_eq!(output.time(), Duration::from_secs(1));
      encoded_channels = output.num_channels() as usize;
      assert_eq!(output.sample_interval(0), sample_interval(48000));
      stream.extend_from_slice(output.buffer());
    }
    (stream, encoded_channels)
  }

  #[test]
  fn aac_round_trip_preserves_the_signal() {
    let (stream, channels) = encode_sines(2, 100, None);
    assert_eq!(channels, 2);
    assert!(!stream.is_empty());
    assert_eq!(&stream[..2], &[0xFF, 0xF1]);
    // 100 chunks of 480 stereo samples: 1.536 Mbit/s raw, ~128 kbit/s AAC.
    let raw_bytes = 100 * 480 * 2 * 2;
    assert!(stream.len() < raw_bytes / 5, "{} bytes of AAC for {} raw", stream.len(), raw_bytes);

    let mut decoder = AudioConverter::new(params(AudioFormat::Decimal));
    let input = TestAnalog::aac(stream, 2, INTERVAL_48K);
    assert!(decoder.needs_conversion(&input));
    decoder.push(&input);
    let output = decoder.pull().expect("nothing decoded");
    assert_eq!(output.num_channels(), 2);
    assert_eq!(output.name(1), "In 1");
    assert_eq!(output.sample_interval(0), sample_interval(48000));
    let decoded = output.data(0);
    // All but the frames still buffered in the encoder, FIFO and parser.
    assert!(decoded.len() > 40_000, "decoded {} samples", decoded.len());
    // A 0.5 amplitude sine has an RMS of about 0.354; AAC keeps it close.
    let tail = &decoded[decoded.len() / 2..];
    let rms = (tail.iter().map(|s| s * s).sum::<f64>() / tail.len() as f64).sqrt();
    assert!((rms - 0.3536).abs() < 0.03, "rms {rms}");
  }

  #[test]
  fn aac_resamples_rates_it_cannot_carry() {
    // 50 kHz is resampled to 48 kHz before encoding.
    let interval = Duration::from_micros(20);
    let mut converter = AudioConverter::new(params(AudioFormat::AAC));
    let mut stream = Vec::new();
    let mut encoded = 0;
    for chunk in 0..50 {
      converter.push(&TestAnalog::double(vec![sine(440.0, 50000.0, 500, chunk * 500)], interval));
      let output = converter.pull().expect("every chunk should produce an output");
      assert_eq!(output.sample_interval(0), sample_interval(48000));
      encoded += output.encoded_count();
      stream.extend_from_slice(output.buffer());
    }
    // 25000 samples at 50 kHz resample to 24000 at 48 kHz, less what the
    // resampler is still holding.
    assert!((23_900..=24_000).contains(&encoded), "encoded {encoded} samples");
    // ADTS sampling_frequency_index 3 is 48 kHz.
    assert_eq!((stream[2] >> 2) & 0xF, 3);

    let mut decoder = AudioConverter::new(params(AudioFormat::Decimal));
    decoder.push(&TestAnalog::aac(stream, 1, interval));
    let decoded = decoder.pull().expect("nothing decoded");
    // 50 chunks of 500 samples at 50 kHz is half a second: about 24000
    // samples at 48 kHz, less what's still buffered.
    let samples = decoded.data(0).len();
    assert!((20_000..=24_000).contains(&samples), "decoded {samples} samples");
  }

  #[test]
  fn aac_bitrate_sets_the_stream_size() {
    // 200 chunks of 480 samples: two seconds of stereo.
    let (low, _) = encode_sines(2, 200, Some(32_000));
    let (high, _) = encode_sines(2, 200, Some(256_000));
    // Near 8 kB and 64 kB; the encoder only approximates its target.
    assert!(low.len() < 12_000, "{} bytes at 32 kbit/s", low.len());
    assert!(high.len() > 3 * low.len(), "{} vs {} bytes", high.len(), low.len());
  }

  #[test]
  fn aac_drops_channels_beyond_what_it_supports() {
    let (stream, channels) = encode_sines(10, 20, None);
    assert_eq!(channels, 8);
    // ADTS channel_configuration 7 means 8 channels.
    let config = ((stream[2] & 1) << 2) | (stream[3] >> 6);
    assert_eq!(config, 7);
  }
}
