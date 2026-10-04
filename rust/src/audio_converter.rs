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

      let frame = if !self.writable.is_empty() {
        let frame = self.writable.pop_front().unwrap();
        if (*frame).nb_samples < nb_samples {
          (*frame).nb_samples = nb_samples;
          let ret = ffi::av_frame_get_buffer(frame, 0);
          assert!(ret >= 0, "ffi::av_frame_get_buffer: {}", av_error_string(ret));
        }
        frame
      } else {
        let frame = ffi::av_frame_alloc();
        match self.params {
          FramePoolParams::Audio { layout, format, samplerate } => {
            ffi::av_channel_layout_copy(&mut (*frame).ch_layout, &layout);
            (*frame).format = format as i32;
            (*frame).sample_rate = samplerate;
          }
        };
        frame
      };

      match self.params {
        FramePoolParams::Audio { .. } => {
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

fn get_codec_config<T>(codec: *const ffi::AVCodec, config: ffi::AVCodecConfig) -> &'static [T] {
  unsafe {
    let mut count = 0;
    let mut vals: *const std::ffi::c_void  = std::ptr::null_mut();
    let ret = ffi::avcodec_get_supported_config(std::ptr::null_mut(), codec, config,
                                       0, &mut vals, &mut count);
    assert!(ret >= 0, "avcodec_get_supported_config error: {}", av_error_string(ret));
    std::slice::from_raw_parts(vals as *const T, count as usize)
  }
}

fn sample_format_for_codec(src_format: ffi::AVSampleFormat, codec: *const ffi::AVCodec) -> ffi::AVSampleFormat {
  let mut formats: *const std::ffi::c_void  = std::ptr::null_mut();
  let mut count = 0;
  let formats = unsafe {
    let ret = ffi::avcodec_get_supported_config(std::ptr::null_mut(), codec, AV_CODEC_CONFIG_SAMPLE_FORMAT,
                                       0, &mut formats, &mut count);
    assert!(ret >= 0, "avcodec_get_supported_config error: {}", av_error_string(ret));
    std::slice::from_raw_parts(formats as *const ffi::AVSampleFormat, count as usize)
  };
  
  if formats.contains(&src_format) {
    src_format
  } else {
    match formats.first() {
      Some(s) => { *s },
      None => panic!("No codec sample rates")
    }
  }
}

fn sample_rate_for_codec(src_frequency: i32, codec: *const ffi::AVCodec) -> i32 {
  let mut rates: *const std::ffi::c_void  = std::ptr::null_mut();
  let mut count = 0;
  let rates = unsafe {
    let ret = ffi::avcodec_get_supported_config(std::ptr::null_mut(), codec, AV_CODEC_CONFIG_SAMPLE_RATE,
                                       0, &mut rates, &mut count);
    assert!(ret >= 0, "avcodec_get_supported_config error: {}", av_error_string(ret));
    std::slice::from_raw_parts(rates as *const std::os::raw::c_int, count as usize)
  };
  let selected = rates.iter().copied().min_by_key(|r| (r - src_frequency).abs());
  match selected {
    Some(s) => { s },
    None => panic!("No codec sample rates")
  }
}

fn encoder_time_base(frame_interval: Duration) -> ffi::AVRational {
  unsafe { ffi::av_d2q(frame_interval.as_secs_f64(), 65535) }
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
        self.decoder = None;
      }
      if let Some(mut encoder) = self.encoder {
        ffi::avcodec_free_context(&mut encoder);
        ffi::av_audio_fifo_free(self.fifo);
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
      self.out_buffer.clear();
      self.slice_to_pts.clear();
      self.need_key_frame = false;
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
      let dst_sample_format = codec.map(|codec| {
        let vals: &[ffi::AVSampleFormat] = get_codec_config(codec, ffi::AVCodecConfig::AV_CODEC_CONFIG_SAMPLE_FORMAT);
        if vals.contains(&src_sample_format) {
          src_sample_format
        } else {
          vals[0]
        }
      }).unwrap_or(audio_format_to_sample_format(dst_audio_format));

      let dst_sample_rate = codec.map(|codec| {
        let vals: &[i32] = get_codec_config(codec, ffi::AVCodecConfig::AV_CODEC_CONFIG_SAMPLE_RATE);
        if vals.contains(&src_sample_rate) {
          src_sample_rate
        } else {
          vals[0]
        }
      }).unwrap_or(self.params.samplerate.unwrap_or(src_sample_rate));
      self.dst_sample_format = dst_sample_format;
      self.dst_sample_rate = dst_sample_rate;
      self.dst_sample_interval = Duration::from_nanos(1_000_000_000/(self.dst_sample_rate as u64));

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

      self.single_samplers = self.input_channels.range.clone().map(|_| {
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
        sampler
      }).collect();

      let reducer_sample_format = ffi::av_get_packed_sample_fmt(dst_sample_format);
      let ret = ffi::swr_alloc_set_opts2(
        &mut self.reducer_sampler,
        &self.layout,
        dst_sample_format,
        dst_sample_rate,
        &self.layout,
        reducer_sample_format,
        src_sample_rate,
        0,
        std::ptr::null_mut(),
      );
      assert!(ret >= 0, "swr_alloc_set_opts2: {}", av_error_string(ret));

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
        let out_bps = ffi::av_get_bytes_per_sample(self.dst_sample_format);
        for (ui, _) in self.input_channels.range.clone().enumerate() {
          let sample_time = &mut self.sample_times[ui];
          let sampler = self.single_samplers[ui];
          let in_samples = in_counts[ui];
          if in_samples == 0 {
            continue;
          }
          let out_samples = ffi::swr_get_out_samples(sampler, in_samples);
          let out_bytes = out_bps*out_samples;

          let buffer = &mut self.sample_buffers[ui];
          let old_length = buffer.len();
          buffer.resize(old_length + out_bytes as usize, 0);
          let buffer_slice = &mut buffer.as_mut_slice()[old_length..];

          *sample_time = sample_time.or_else(|| {
            Some(time - self.input_channels.sample_interval*((in_samples-1) as u32))
          });

          let ret = ffi::swr_convert(sampler, &mut buffer_slice.as_mut_ptr(), out_samples, &in_ptrs[ui], in_samples as i32);
          assert!(ret >= 0, "swr_convert: {}", av_error_string(ret));
          buffer.resize(old_length + (ret as usize), 0);
        }

        let latest = self.sample_times.iter().max().cloned();
        let mut starts = Vec::<usize>::new();
        starts.resize(self.sample_times.len(), 0);

        if let Some(Some(latest)) = latest {
          for (ui, _) in self.input_channels.range.clone().enumerate() {
            let Some(time) = self.sample_times[ui].as_mut() else {
              continue
            };
            let start = &mut starts[ui];
            *start = ((latest - *time).as_nanos()/self.input_channels.sample_interval.as_nanos()) as usize;
          }
        }

        let sample_slices: Vec<_> = self.sample_buffers.iter().zip(starts.iter()).map(|(b, i)| {
          &b[(i*out_bps as usize)..]
        }).collect();

        let min_size = sample_slices.iter().map(|b|b.len()).min().unwrap_or(0);
        if min_size > 0 {
          let ptrs: Vec<*const u8> = sample_slices.iter().map(|b| b.as_ptr()).collect();
          let sizes = vec![min_size as i32; ptrs.len()];
          self.convert_samples(self.reducer_sampler, ptrs.as_slice(), sizes.as_slice(), time, true);
        }

        for ((start, buf), time) in starts.iter().zip(self.sample_buffers.iter_mut()).zip(self.sample_times.iter_mut()) {
          let discarded = start+min_size;
          buf.drain(..(discarded*out_bps as usize));
          *time = (*time).map(|t| t + (discarded as u32)*self.input_channels.sample_interval);
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
      if input_channels.sample_interval.is_zero() {
        if self.rejection_logged {
          println!("Sample interval is required in AudioConverter input");
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
      if !self.encoder_configured {
        let src_sample_format = analog_to_sample_format(self.input_channels.format);
        let src_sample_rate = (1_000_000_000/self.input_channels.sample_interval.as_nanos()) as i32;
        self.configure_encoder(src_sample_format, src_sample_rate);
      }
      if !self.sampler_configured {
        let src_sample_format = analog_to_sample_format(self.input_channels.format);
        let src_sample_rate = (1_000_000_000/self.input_channels.sample_interval.as_nanos()) as i32;
        self.configure_resampler(src_sample_format, src_sample_rate);
      }

      let pointers: Vec<_> = self.input_channels.range.clone().map(|i| analog_data_ptr(input, i)).collect();
      let counts: Vec<_> = self.input_channels.range.clone().map(|i| input.count(i) as i32).collect();
      self.convert_samples(self.multi_sampler, pointers.as_slice(), counts.as_slice(), data.time(), false);
    }
  }

  fn parser_to_decoder(&mut self, decoder: *mut ffi::AVCodecContext) {
    unsafe {
      let mut offset = 0;
      while offset + (AV_INPUT_BUFFER_PADDING_SIZE as usize) < self.in_buffer.len() {
        let pts = loop {
          let (end, pts) = self.slice_to_pts.front().expect("Ran out of pts while parsing");
          if offset < *end {
            break *pts;
          }
          self.slice_to_pts.pop_front();
        };

        let used = ffi::av_parser_parse2(
          self.parser, decoder, 
          &mut (*self.parser_packet).data,
          &mut (*self.parser_packet).size,
          self.in_buffer.as_ptr().add(offset),
          (self.in_buffer.len() - (AV_INPUT_BUFFER_PADDING_SIZE as usize) - offset) as i32,
          pts, ffi::AV_NOPTS_VALUE, 
          self.num_input_bytes
        );
        offset += used as usize;
        self.num_input_bytes += used as i64;

        if (*self.parser_packet).size > 0 {
          let discard = false;//self.need_key_frame && (*self.parser).pict_type != ffi::AVPictureType::AV_PICTURE_TYPE_I as i32;
          if !discard {
            self.need_key_frame = false;
            (*self.parser_packet).pts = (*self.parser).pts;
            let ret = ffi::avcodec_send_packet(decoder, self.parser_packet);
            if ret == AVERROR_EAGAIN {
              return;
            } 
            assert!(ret >= 0, "avcodec_send_packet {}", av_error_string(ret));
          }
        }
        
        if used == 0 && (*self.parser_packet).size == 0 {
          break;
        }
      }
      if offset > 0 {
        self.in_buffer.drain(0..offset);
        for (end, _) in self.slice_to_pts.iter_mut() {
          *end -= offset;
        }
      }
    }
  }

  pub fn pull(&mut self) -> Option<AudioOutput<'_>> {
    unsafe {
      let mut frame = if let Some(decoder) = self.decoder {
        self.parser_to_decoder(decoder);

        let ret = ffi::avcodec_receive_frame(decoder, self.decoder_frame);
        if ret == AVERROR_EAGAIN || ret == AVERROR_EOF {
          return None;                   // needs more input / fully drained
        }
        assert!(ret >= 0, "avcodec_receive_frame: {}", av_error_string(ret));

        let src_sample_format = std::mem::transmute::<i32, ffi::AVSampleFormat>((*self.decoder_frame).format);
        let src_sample_rate = (*self.decoder_frame).sample_rate;
        let nb_samples = (*self.decoder_frame).nb_samples;

        if self.encoder.is_some() && src_sample_format == self.dst_sample_format && src_sample_rate == self.dst_sample_rate {
          self.decoder_frame
        } else {
          self.configure_encoder(src_sample_format, src_sample_rate);
          self.configure_resampler(src_sample_format, src_sample_rate);

          let frame = self.dst_frame_pool.get_writable(nb_samples);
          ffi::swr_convert_frame(self.multi_sampler, frame, self.decoder_frame);
          self.dst_frame_pool.push_pending(frame);
          self.dst_frame_pool.get_pending().unwrap()
        }
      } else {
        match self.dst_frame_pool.get_pending() {
          Some(f) => { f }
          None => {return None;}
        }
      };

      let pts = match self.pts_to_time.iter().position(|(pts, _)| pts == &(*frame).pts) {
        Some(i) => {
          let temp = self.pts_to_time[i];
          self.pts_to_time.remove(i);
          temp.1
        },
        None => self.api.time()
      };

      self.out_buffer.clear();
      if let Some(encoder) = self.encoder {
        let nb_samples = (*frame).nb_samples;
        let ret = ffi::av_audio_fifo_write(self.fifo, (*frame).extended_data as *const *mut c_void, nb_samples);
        assert!(ret == nb_samples, "av_audio_fifo_write {}", av_error_string(ret));
        ffi::av_frame_unref(frame);
        if frame != self.decoder_frame {
          ffi::av_frame_free(&mut frame);
        }

        let mut encoded_count = 0;
        let frame_size = (*encoder).frame_size;
        while ffi::av_audio_fifo_size(self.fifo) >= frame_size {
          let ret = ffi::av_frame_make_writable(self.fifo_frame);
          assert!(ret >= 0, "av_frame_make_writable {}", av_error_string(ret));

          (*self.fifo_frame).nb_samples = frame_size;
          let read = ffi::av_audio_fifo_read(self.fifo, (*self.fifo_frame).extended_data as *const *mut c_void, frame_size);
          assert!(read == frame_size, "av_audio_fifo_read {}", av_error_string(ret));
          encoded_count += frame_size;
        
          let ret = ffi::avcodec_send_frame(encoder, self.fifo_frame);
          assert!(ret >= 0, "avcodec_send_frame: {}", av_error_string(ret));
        }

        loop {
          let ret = ffi::avcodec_receive_packet(encoder, self.packet);
          if ret == AVERROR_EAGAIN || ret == AVERROR_EOF {
            break;
          }
          assert!(ret >= 0, "Error during encoding: {}", av_error_string(ret));

          let slice = std::slice::from_raw_parts((*self.packet).data, (*self.packet).size as usize);
          self.out_buffer.extend_from_slice(slice);
        }

        Some(AudioOutput { converter: self, frame: None, time: pts, encoded_count })
      } else {
        Some(AudioOutput { converter: self, frame: Some(frame), time: pts, encoded_count: 0 })
      }
    }
  }
}

impl Drop for AudioConverter {
  fn drop(&mut self) {
    self.cleanup();
  }
}