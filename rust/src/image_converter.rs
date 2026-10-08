use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;
use std::ffi::{CStr};

use ffmpeg_sys_next::{self as ffi, AV_INPUT_BUFFER_PADDING_SIZE, AVCodecContext, AVCodecParserContext, AVPixelFormat};

use crate::api::{ImageData, ImageFormat, NodeData, ThalamusAPIThreadSafe};
use crate::audio_converter::get_codec_config;
use crate::frame_pool::{FramePool, FramePoolParams};

const fn mktag(a: u8, b: u8, c: u8, d: u8) -> i32 {
  (a as i32) | ((b as i32) << 8) | ((c as i32) << 16) | ((d as i32) << 24)
}

const AVERROR_EAGAIN: i32 = -ffi::EAGAIN;
pub(crate) const AVERROR_EOF: i32 = -mktag(b'E', b'O', b'F', b' ');

fn encoder_time_base(frame_interval: Duration) -> ffi::AVRational {
  unsafe { ffi::av_d2q(frame_interval.as_secs_f64(), 65535) }
}

/// The requested output format. None (passthrough) keeps the input's.
#[derive(Debug,Clone,Copy,PartialEq)]
pub enum VideoFormat {
  /// Encoded input is output in the pixel format its decoder produces; raw
  /// input passes through.
  Decoded,
  Image(ImageFormat),
}

#[derive(Debug,Clone,Copy,PartialEq)]
pub struct ConverterParams {
  pub format: Option<VideoFormat>,
  pub width: Option<i32>,
  pub height: Option<i32>,
  /// MPEG4 quantizer scale, 1-31, lower is better.
  pub mpeg4_quality: Option<i32>,
  /// H264 quantization parameter (QP), 1-51, lower is better.
  pub h264_quality: Option<i32>,
  /// VP9 constant quality level (CRF), 0-63, lower is better.
  pub vp9_quality: Option<i32>,
  /// Each encoded input message holds exactly one whole frame. The parser is
  /// told so and given one message at a time, which saves it from waiting for
  /// the next frame's start before returning a packet: one frame less latency.
  pub complete_frames: bool,
}

fn is_compressed(format: ImageFormat) -> bool {
  matches!(format, ImageFormat::MPEG4 | ImageFormat::H264 | ImageFormat::VP9)
}

fn format_codec(format: ImageFormat) -> ffi::AVCodecID {
  match format {
    ImageFormat::MPEG4 => ffi::AVCodecID::AV_CODEC_ID_MPEG4,
    // Encoded with FFmpeg's libopenh264 wrapper (FFmpeg has no H.264 encoder
    // of its own), decoded with FFmpeg's h264 decoder.
    ImageFormat::H264 => ffi::AVCodecID::AV_CODEC_ID_H264,
    // Encoded with FFmpeg's libvpx-vp9 wrapper (FFmpeg has no software VP9
    // encoder of its own), decoded with FFmpeg's vp9 decoder.
    ImageFormat::VP9 => ffi::AVCodecID::AV_CODEC_ID_VP9,
    _ => panic!("Unsupported format")
  }
}

/// Limits of an encoder that FFmpeg doesn't report.
struct CodecLimits {
  /// The largest width or height the bitstream can describe.
  max_dimension: i32,
  /// The largest frame, in pixels, the encoder accepts.
  max_pixels: i64,
  /// The quantizer values `quality` may select, in the codec's own scale
  /// (lower is better), and the one used when no quality is set.
  qscale: (i32, i32),
  default_quality: i32,
}

fn codec_limits(codec_id: ffi::AVCodecID) -> CodecLimits {
  match codec_id {
    // MPEG-4 Part 2 headers store the size in 13 bits.
    ffi::AVCodecID::AV_CODEC_ID_MPEG4 => CodecLimits {
      max_dimension: 8191,
      max_pixels: i64::MAX,
      qscale: (1, 31),
      default_quality: 5,
    },
    // OpenH264 accepts frames up to 36864 macroblocks (H.264 level 5.1), e.g.
    // 4096x2304; quality is the QP.
    ffi::AVCodecID::AV_CODEC_ID_H264 => CodecLimits {
      max_dimension: 8192,
      max_pixels: 36864 * 256,
      qscale: (1, 51),
      default_quality: 18,
    },
    // libvpx's maximum frame size; quality is the CRF.
    ffi::AVCodecID::AV_CODEC_ID_VP9 => CodecLimits {
      max_dimension: 16384,
      max_pixels: i64::MAX,
      qscale: (0, 63),
      default_quality: 24,
    },
    _ => panic!("Unsupported codec {:?}", codec_id)
  }
}

/// The AVFrame/AVCodecContext quality for `quality` in the codec's own scale:
/// FFmpeg's lambda for quantizer-scale codecs (MPEG4), unused (0) for H264,
/// whose QP is set through qmin/qmax instead.
fn frame_quality(codec_id: ffi::AVCodecID, quality: i32) -> i32 {
  match codec_id {
    ffi::AVCodecID::AV_CODEC_ID_H264 | ffi::AVCodecID::AV_CODEC_ID_VP9 => 0,
    _ => ffi::FF_QP2LAMBDA * quality,
  }
}

/// Sets an encoder private option (e.g. libvpx-vp9's "crf") before it's
/// opened.
unsafe fn set_encoder_option(context: *mut AVCodecContext, name: &CStr, value: &str) {
  let value = std::ffi::CString::new(value).expect("option value contains a NUL");
  let ret = unsafe { ffi::av_opt_set((*context).priv_data, name.as_ptr(), value.as_ptr(), 0) };
  assert!(ret >= 0, "setting {:?} to {:?}: {}", name, value, av_error_string(ret));
}

/// FFmpeg's encoder for `codec_id`, if this build has one. H264's is
/// libopenh264, which only works while Cisco's OpenH264 binary is available
/// (see crate::openh264).
fn find_encoder(codec_id: ffi::AVCodecID) -> Option<*const ffi::AVCodec> {
  let codec = unsafe { ffi::avcodec_find_encoder(codec_id) };
  (!codec.is_null()).then_some(codec)
}

/// `src` if the encoder takes it, otherwise the supported format that loses
/// the least converting from `src`.
fn pix_format_for_codec(src: AVPixelFormat, codec: *const ffi::AVCodec) -> AVPixelFormat {
  let formats: &[AVPixelFormat] = get_codec_config(codec, ffi::AVCodecConfig::AV_CODEC_CONFIG_PIX_FORMAT);
  if formats.is_empty() || formats.contains(&src) {
    return src;
  }
  let mut list = formats.to_vec();
  list.push(AVPixelFormat::AV_PIX_FMT_NONE);
  unsafe { ffi::avcodec_find_best_pix_fmt_of_list(list.as_ptr(), src, 0, std::ptr::null_mut()) }
}

/// `width` x `height` scaled down (keeping the aspect ratio) to the codec's
/// largest frame, capped at its maximum dimension and rounded down to whole
/// chroma blocks of `pix`.
fn size_for_codec(codec_id: ffi::AVCodecID, pix: AVPixelFormat, width: i32, height: i32) -> (i32, i32) {
  let limits = codec_limits(codec_id);
  let desc = unsafe { ffi::av_pix_fmt_desc_get(pix) };
  assert!(!desc.is_null(), "no descriptor for {:?}", pix);
  let (align_w, align_h) = unsafe { (1 << (*desc).log2_chroma_w, 1 << (*desc).log2_chroma_h) };
  let pixels = width as i64 * height as i64;
  let (width, height) = if pixels > limits.max_pixels {
    let scale = (limits.max_pixels as f64 / pixels as f64).sqrt();
    ((width as f64 * scale) as i32, (height as f64 * scale) as i32)
  } else {
    (width, height)
  };
  let fit = |size: i32, align: i32| (size.min(limits.max_dimension) / align * align).max(align);
  (fit(width, align_w), fit(height, align_h))
}

/// The time base for frames `frame_interval` apart, or for the nearest frame
/// rate the encoder supports if it only supports some.
fn time_base_for_codec(frame_interval: Duration, codec: *const ffi::AVCodec) -> ffi::AVRational {
  let rates: &[ffi::AVRational] = get_codec_config(codec, ffi::AVCodecConfig::AV_CODEC_CONFIG_FRAME_RATE);
  let fps = 1.0 / frame_interval.as_secs_f64();
  let nearest = rates.iter()
    .filter(|r| r.num > 0 && r.den > 0)
    .min_by(|a, b| {
      let distance = |r: &ffi::AVRational| (r.num as f64 / r.den as f64 - fps).abs();
      distance(a).total_cmp(&distance(b))
    });
  match nearest {
    Some(rate) => ffi::AVRational { num: rate.den, den: rate.num },
    None => encoder_time_base(frame_interval),
  }
}

fn av_error_string(ret: i32) -> String {
  let mut buf = [0i8; ffi::AV_ERROR_MAX_STRING_SIZE];
  let rc = unsafe { ffi::av_strerror(ret, buf.as_mut_ptr(), buf.len()) };
  if rc == 0 {
    unsafe { CStr::from_ptr(buf.as_ptr()) }
      .to_string_lossy()
      .to_string()
  } else {
    format!("error code {}", ret)
  }
}

// 16-bit formats are in the platform's native byte order.
const GRAY16_NATIVE: AVPixelFormat = if cfg!(target_endian = "big") {
  AVPixelFormat::AV_PIX_FMT_GRAY16BE
} else {
  AVPixelFormat::AV_PIX_FMT_GRAY16LE
};
const RGB48_NATIVE: AVPixelFormat = if cfg!(target_endian = "big") {
  AVPixelFormat::AV_PIX_FMT_RGB48BE
} else {
  AVPixelFormat::AV_PIX_FMT_RGB48LE
};

fn pix_to_height(height: i32, pix: AVPixelFormat) -> [i32; 4] {
  match pix {
    AVPixelFormat::AV_PIX_FMT_GRAY8 => [height, 0, 0, 0],
    AVPixelFormat::AV_PIX_FMT_RGB24 => [height, 0, 0, 0],
    AVPixelFormat::AV_PIX_FMT_YUYV422 => [height, 0, 0, 0],
    // Chroma planes of 4:2:0 formats have ceil(height / 2) rows.
    AVPixelFormat::AV_PIX_FMT_YUV420P => [height, (height + 1)/2, (height + 1)/2, 0],
    AVPixelFormat::AV_PIX_FMT_YUVJ420P => [height, (height + 1)/2, (height + 1)/2, 0],
    AVPixelFormat::AV_PIX_FMT_NV12 => [height, (height + 1)/2, 0, 0],
    AVPixelFormat::AV_PIX_FMT_BGR24 => [height, 0, 0, 0],
    AVPixelFormat::AV_PIX_FMT_GRAY16LE | AVPixelFormat::AV_PIX_FMT_GRAY16BE => [height, 0, 0, 0],
    AVPixelFormat::AV_PIX_FMT_RGB48LE | AVPixelFormat::AV_PIX_FMT_RGB48BE => [height, 0, 0, 0],
    _ => panic!("Unsupported pixel format: {:?}", pix)
  }
}

fn pix_to_num_planes(pix: AVPixelFormat) -> i32 {
  match pix {
    AVPixelFormat::AV_PIX_FMT_GRAY8 => 1,
    AVPixelFormat::AV_PIX_FMT_RGB24 => 1,
    AVPixelFormat::AV_PIX_FMT_YUYV422 => 1,
    AVPixelFormat::AV_PIX_FMT_YUV420P => 3,
    AVPixelFormat::AV_PIX_FMT_YUVJ420P => 3,
    AVPixelFormat::AV_PIX_FMT_NV12 => 2,
    AVPixelFormat::AV_PIX_FMT_BGR24 => 1,
    AVPixelFormat::AV_PIX_FMT_GRAY16LE | AVPixelFormat::AV_PIX_FMT_GRAY16BE => 1,
    AVPixelFormat::AV_PIX_FMT_RGB48LE | AVPixelFormat::AV_PIX_FMT_RGB48BE => 1,
    _ => panic!("Unsupported pixel format: {:?}", pix)
  }
}

fn image_to_pix(format: ImageFormat) -> AVPixelFormat {
  match format {
    ImageFormat::Gray => AVPixelFormat::AV_PIX_FMT_GRAY8,
    ImageFormat::RGB => AVPixelFormat::AV_PIX_FMT_RGB24,
    ImageFormat::YUYV422 => AVPixelFormat::AV_PIX_FMT_YUYV422,
    ImageFormat::YUV420P => AVPixelFormat::AV_PIX_FMT_YUV420P,
    ImageFormat::YUVJ420P => AVPixelFormat::AV_PIX_FMT_YUVJ420P,
    ImageFormat::NV12 => AVPixelFormat::AV_PIX_FMT_NV12,
    ImageFormat::BGR => AVPixelFormat::AV_PIX_FMT_BGR24,
    ImageFormat::MPEG1 => AVPixelFormat::AV_PIX_FMT_YUV420P,
    ImageFormat::MPEG4 => AVPixelFormat::AV_PIX_FMT_YUV420P,
    ImageFormat::H264 => AVPixelFormat::AV_PIX_FMT_YUV420P,
    ImageFormat::VP9 => AVPixelFormat::AV_PIX_FMT_YUV420P,
    ImageFormat::MJPEG => panic!("No Pixel format"),
    ImageFormat::Gray16 => GRAY16_NATIVE,
    ImageFormat::RGB16 => RGB48_NATIVE,
  }
}

/// The raw format with pixel format `pix`, if there is one.
fn pix_to_image(pix: AVPixelFormat) -> Option<ImageFormat> {
  match pix {
    AVPixelFormat::AV_PIX_FMT_GRAY8 => Some(ImageFormat::Gray),
    AVPixelFormat::AV_PIX_FMT_RGB24 => Some(ImageFormat::RGB),
    AVPixelFormat::AV_PIX_FMT_YUYV422 => Some(ImageFormat::YUYV422),
    AVPixelFormat::AV_PIX_FMT_YUV420P => Some(ImageFormat::YUV420P),
    AVPixelFormat::AV_PIX_FMT_YUVJ420P => Some(ImageFormat::YUVJ420P),
    AVPixelFormat::AV_PIX_FMT_NV12 => Some(ImageFormat::NV12),
    AVPixelFormat::AV_PIX_FMT_BGR24 => Some(ImageFormat::BGR),
    GRAY16_NATIVE => Some(ImageFormat::Gray16),
    RGB48_NATIVE => Some(ImageFormat::RGB16),
    _ => None,
  }
}

/// An encoded (MPEG4, H264 or VP9) output. Its bytes are in the codec state's buffer, so
/// it holds the codec lock while it's read, which only holds up other pulls.
pub struct EncodedImage<'a> {
  codec: MutexGuard<'a, CodecState>,
  frame_interval: Duration,
  time: Duration,
  format: ImageFormat,
  width: i32,
  height: i32,
}

impl<'a> NodeData for EncodedImage<'a> {
  fn time(&self) -> Duration {
    self.time
  }

  fn image(&self) -> Option<&dyn ImageData> {
    Some(self)
  }
}

impl<'a> ImageData for EncodedImage<'a> {
  fn plane(&self, _channel: i32) -> &[u8] {
    &self.codec.out_buffer
  }

  fn num_planes(&self) -> u64 {
    1
  }

  fn format(&self) -> ImageFormat {
    self.format
  }

  fn width(&self) -> u64 {
    self.width as u64
  }

  fn height(&self) -> u64 {
    self.height as u64
  }

  fn frame_interval(&self) -> Duration {
    self.frame_interval
  }
}

/// A raw output. It owns a reference to its frame, so it holds no lock.
struct RawImage {
  frame: *mut ffi::AVFrame,
  num_planes: i32,
  format: ImageFormat,
  plane_heights: [i32; 4],
  frame_interval: Duration,
  time: Duration,
}

impl NodeData for RawImage {
  fn time(&self) -> Duration {
    self.time
  }

  fn image(&self) -> Option<&dyn ImageData> {
    Some(self)
  }
}

impl ImageData for RawImage {
  fn plane(&self, i: i32) -> &[u8] {
    let i = i as usize;
    unsafe {
      let data = (*self.frame).data[i];
      let linesize = (*self.frame).linesize[i];
      let height = self.plane_heights[i];
      let size = (linesize*height) as usize;
      std::ptr::slice_from_raw_parts(data, size).as_ref().unwrap()
    }
  }

  fn num_planes(&self) -> u64 {
    self.num_planes as u64
  }

  fn format(&self) -> ImageFormat {
    self.format
  }

  fn width(&self) -> u64 {
    unsafe { (*self.frame).width as u64 }
  }

  fn height(&self) -> u64 {
    unsafe { (*self.frame).height as u64 }
  }

  fn frame_interval(&self) -> Duration {
    self.frame_interval
  }
}

impl Drop for RawImage {
  fn drop(&mut self) {
    unsafe { ffi::av_frame_free(&mut self.frame) };
  }
}

/// The input's format and size. A change starts the converter over.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ImageSource {
  format: ImageFormat,
  pix: AVPixelFormat,
  width: i32,
  height: i32,
}

/// The output format and size, chosen from the parameters and the input.
#[derive(Debug, Clone, Copy)]
struct OutputConfig {
  format: ImageFormat,
  pix: AVPixelFormat,
  width: i32,
  height: i32,
  /// The encoder to use, if any. Only pull opens it.
  codec_id: Option<ffi::AVCodecID>,
  quality: i32,
}

/// The output for `source`, which for encoded input is the decoded frames'
/// format and size. Encoded output is limited to what its encoder supports.
fn choose_output(params: &ConverterParams, source: &ImageSource) -> OutputConfig {
  let format = match params.format {
    None => source.format,
    // The decoder's pixel format, or YUV420P if no raw format matches it.
    Some(VideoFormat::Decoded) if is_compressed(source.format) =>
      pix_to_image(source.pix).unwrap_or(ImageFormat::YUV420P),
    Some(VideoFormat::Decoded) => source.format,
    Some(VideoFormat::Image(format)) => format,
  };
  let width = params.width.unwrap_or(source.width);
  let height = params.height.unwrap_or(source.height);
  if !is_compressed(format) {
    return OutputConfig {
      format,
      pix: image_to_pix(format),
      width,
      height,
      codec_id: None,
      quality: 0,
    };
  }

  let codec_id = format_codec(format);
  // Without an encoder nothing is encoded anyway (see open_encoder).
  let pix = find_encoder(codec_id)
    .map_or(AVPixelFormat::AV_PIX_FMT_YUV420P, |codec| pix_format_for_codec(source.pix, codec));
  let (width, height) = size_for_codec(codec_id, pix, width, height);
  let limits = codec_limits(codec_id);
  let (qmin, qmax) = limits.qscale;
  let quality = match codec_id {
    ffi::AVCodecID::AV_CODEC_ID_H264 => params.h264_quality,
    ffi::AVCodecID::AV_CODEC_ID_VP9 => params.vp9_quality,
    _ => params.mpeg4_quality,
  }.unwrap_or(limits.default_quality);
  OutputConfig {
    format,
    pix,
    width,
    height,
    codec_id: Some(codec_id),
    quality: quality.clamp(qmin, qmax),
  }
}

fn frame_interval_or_default(interval: Duration) -> Duration {
  if interval.is_zero() { Duration::from_millis(16) } else { interval }
}

/// What push works on, under the input lock: the input's format, the message
/// times, the MPEG4 parser's input and the raw input's scaling. Resetting
/// replaces it with a fresh state of the next generation.
struct InputState {
  params: ConverterParams,
  generation: u64,
  /// Set by the first message after a reset.
  source: Option<ImageSource>,
  /// Raw input's output, chosen with the source. Encoded input's is chosen
  /// by pull from the first decoded frame.
  output: Option<OutputConfig>,
  frame_interval: Duration,

  pts: i64,
  /// (pts, message time, frame interval)
  pts_to_time: VecDeque<(i64, Duration, Duration)>,

  // Encoded input: bytes waiting for the parser, which pull runs.
  parser: *mut AVCodecParserContext,
  parser_packet: *mut ffi::AVPacket,
  in_buffer: Vec<u8>,
  slice_to_pts: VecDeque<(usize, i64)>,
  num_input_bytes: i64,
  need_key_frame: bool,

  // Raw input, scaled here when the output differs.
  scaler: *mut ffi::SwsContext,
}

// SAFETY: the FFmpeg contexts are only used under the input mutex.
unsafe impl Send for InputState {}

impl InputState {
  fn new(params: ConverterParams, generation: u64) -> InputState {
    InputState {
      params,
      generation,
      source: None,
      output: None,
      frame_interval: Duration::from_millis(16),
      pts: 0,
      pts_to_time: VecDeque::new(),
      parser: std::ptr::null_mut(),
      parser_packet: std::ptr::null_mut(),
      in_buffer: vec![],
      slice_to_pts: VecDeque::new(),
      num_input_bytes: 0,
      need_key_frame: true,
      scaler: std::ptr::null_mut(),
    }
  }

  /// Sets up for `source`: the parser for encoded input, or the output,
  /// scaler and frame pool for raw input. Encoded input's output depends on
  /// what the decoder produces, so pull sets it up from the first decoded
  /// frame.
  fn configure(&mut self, source: ImageSource, pool: &Mutex<FramePool>) {
    self.source = Some(source);
    unsafe {
      if is_compressed(source.format) {
        let codec_id = format_codec(source.format);
        self.parser = ffi::av_parser_init(codec_id as i32);
        assert!(!self.parser.is_null(), "Failed to create parser");
        if self.params.complete_frames {
          (*self.parser).flags |= ffi::PARSER_FLAG_COMPLETE_FRAMES as i32;
        }
        self.parser_packet = ffi::av_packet_alloc();
        assert!(!self.parser_packet.is_null(), "Failed to create parser_packet");
        self.in_buffer.resize(AV_INPUT_BUFFER_PADDING_SIZE as usize, 0);
        return;
      }

      let output = choose_output(&self.params, &source);
      // Also used when the formats match, which sws_scale treats as a copy.
      self.scaler = ffi::sws_getContext(
        source.width, source.height, source.pix,
        output.width, output.height, output.pix,
        ffi::SWS_BILINEAR,
        std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
      assert!(!self.scaler.is_null(), "sws_getContext failed");
      *pool.lock().unwrap() = FramePool::new(FramePoolParams::Video {
        format: output.pix,
        width: output.width,
        height: output.height,
      }, self.generation);
      self.output = Some(output);
    }
  }

  /// Appends an encoded message's bytes for the parser.
  fn queue_encoded(&mut self, pts: i64, bytes: &[u8]) {
    // An empty message holds no frame, and as a slice of its own it would
    // stall complete-frames parsing.
    if bytes.is_empty() {
      return;
    }
    let buffer_pos = self.in_buffer.len() - AV_INPUT_BUFFER_PADDING_SIZE as usize;
    self.in_buffer.resize(self.in_buffer.len() + bytes.len(), 0);
    let end = buffer_pos + bytes.len();
    self.in_buffer[buffer_pos..end].copy_from_slice(bytes);
    self.slice_to_pts.push_back((end, pts));
  }

  /// Scales a raw image into a frame of the output format (a copy when the
  /// formats match) and queues it in `pool`. The pool is only locked to take
  /// and queue the frame.
  fn convert_raw(&mut self, pool: &Mutex<FramePool>, pts: i64, input: &dyn ImageData) {
    let frame = pool.lock().unwrap().get_writable(0);
    unsafe {
      // Scale straight from the input's planes; no intermediate copy.
      let source = self.source.expect("convert_raw without a source");
      let heights = pix_to_height(source.height, source.pix);
      let mut src_data = [std::ptr::null::<u8>(); 4];
      let mut src_linesize = [0i32; 4];
      for i in 0..(input.num_planes() as usize).min(4) {
        let plane = input.plane(i as i32);
        src_data[i] = plane.as_ptr();
        src_linesize[i] = if heights[i] > 0 { plane.len() as i32 / heights[i] } else { 0 };
      }
      ffi::sws_scale(
        self.scaler,
        src_data.as_ptr(), src_linesize.as_ptr(),
        0, source.height,
        (*frame).data.as_ptr(), (*frame).linesize.as_ptr());
      (*frame).pts = pts;
    }
    pool.lock().unwrap().push_pending(frame, self.generation);
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

  /// The time and frame interval of the input message that produced `pts`.
  /// Earlier entries belong to messages that produced no frame of their own
  /// and are dropped.
  fn take_time(&mut self, pts: i64) -> Option<(Duration, Duration)> {
    let i = self.pts_to_time.iter().position(|(p, _, _)| *p == pts)?;
    let (_, time, interval) = self.pts_to_time[i];
    self.pts_to_time.drain(..=i);
    Some((time, interval))
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
      if !self.scaler.is_null() {
        ffi::sws_freeContext(self.scaler);
      }
    }
  }
}

/// What pull works on, under the codec lock, which push never takes: the
/// decoder, the encoder and what feeds them. Rebuilt whenever the input
/// side's generation changes.
pub struct CodecState {
  generation: u64,
  source: Option<ImageSource>,
  output: Option<OutputConfig>,
  frame_interval: Duration,

  // Encoded input
  decoder: Option<*mut AVCodecContext>,
  decoder_frame: *mut ffi::AVFrame,
  /// The next parsed packet, copied out of the parser's buffer so decoding
  /// doesn't hold the input lock.
  packet_in: *mut ffi::AVPacket,
  /// Scales decoded frames when the output differs. Created with the output,
  /// from the first decoded frame.
  decoded_scaler: *mut ffi::SwsContext,

  // Encoded output
  encoder: Option<*mut AVCodecContext>,
  /// Opening the encoder failed; not retried until the next reset.
  encoder_failed: bool,
  /// That OpenH264 is missing has been logged.
  openh264_missing_logged: bool,
  packet: *mut ffi::AVPacket,
  out_buffer: Vec<u8>,
}

// SAFETY: the FFmpeg contexts are only used under the codec mutex.
unsafe impl Send for CodecState {}

impl CodecState {
  fn new(generation: u64) -> CodecState {
    CodecState {
      generation,
      source: None,
      output: None,
      frame_interval: Duration::from_millis(16),
      decoder: None,
      decoder_frame: std::ptr::null_mut(),
      packet_in: std::ptr::null_mut(),
      decoded_scaler: std::ptr::null_mut(),
      encoder: None,
      packet: std::ptr::null_mut(),
      out_buffer: vec![],
      encoder_failed: false,
      openh264_missing_logged: false,
    }
  }

  fn open_decoder(&mut self) -> *mut AVCodecContext {
    if let Some(decoder) = self.decoder {
      return decoder;
    }
    let source = self.source.expect("open_decoder without a source");
    unsafe {
      let codec_id = format_codec(source.format);
      let codec = ffi::avcodec_find_decoder(codec_id);
      if codec.is_null() {
        panic!("no {:?} decoder in this FFmpeg build", codec_id);
      }
      let mut context = ffi::avcodec_alloc_context3(codec);
      assert!(!context.is_null(), "avcodec_alloc_context3 failed");
      (*context).pkt_timebase = encoder_time_base(self.frame_interval);
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

  /// The encoder for `output`, opened on first use. None if it can't be
  /// opened, e.g. H264 while OpenH264 is disabled or not downloaded; that's
  /// logged once and not retried until the converter is reset.
  fn open_encoder(&mut self, codec_id: ffi::AVCodecID, output: &OutputConfig) -> Option<*mut AVCodecContext> {
    if let Some(encoder) = self.encoder {
      return Some(encoder);
    }
    if self.encoder_failed {
      return None;
    }
    // H264 needs Cisco's OpenH264 binary. Until it's downloaded frames are
    // dropped, and it's checked again for every frame, so a download that
    // finishes later is used without a reset.
    if codec_id == ffi::AVCodecID::AV_CODEC_ID_H264 && !crate::openh264::library_present() {
      if !self.openh264_missing_logged {
        println!("ImageConverter: OpenH264 isn't downloaded, dropping H264 frames until it is");
        self.openh264_missing_logged = true;
      }
      return None;
    }
    unsafe {
      let Some(codec) = find_encoder(codec_id) else {
        println!("ImageConverter: this FFmpeg build has no {:?} encoder", codec_id);
        self.encoder_failed = true;
        return None;
      };
      let mut context = ffi::avcodec_alloc_context3(codec);
      assert!(!context.is_null(), "avcodec_alloc_context3 failed");
      let time_base = time_base_for_codec(self.frame_interval, codec);
      (*context).width = output.width;
      (*context).height = output.height;
      (*context).pix_fmt = output.pix;
      (*context).time_base = time_base;
      (*context).framerate = ffi::AVRational { num: time_base.den, den: time_base.num };
      (*context).gop_size = time_base.den/time_base.num;
      match codec_id {
        // A constant QP: libopenh264 has no quantizer-scale mode, but its
        // quality rate control stays within qmin..qmax.
        ffi::AVCodecID::AV_CODEC_ID_H264 => {
          (*context).qmin = output.quality;
          (*context).qmax = output.quality;
        }
        // Constant quality (CRF with no bit rate target), tuned for live use:
        // realtime speed, no frame lag (so one packet per frame, with no
        // alt-ref superframes) and row-based multithreading.
        ffi::AVCodecID::AV_CODEC_ID_VP9 => {
          (*context).bit_rate = 0;
          set_encoder_option(context, c"crf", &output.quality.to_string());
          set_encoder_option(context, c"deadline", "realtime");
          set_encoder_option(context, c"cpu-used", "8");
          set_encoder_option(context, c"lag-in-frames", "0");
          set_encoder_option(context, c"row-mt", "1");
        }
        _ => {
          (*context).flags |= ffi::AV_CODEC_FLAG_QSCALE as i32;
          (*context).global_quality = frame_quality(codec_id, output.quality);
        }
      }
      (*context).max_b_frames = 0;
      let ret = ffi::avcodec_open2(context, codec, std::ptr::null_mut());
      if ret < 0 {
        ffi::avcodec_free_context(&mut context);
        println!("ImageConverter: opening the {:?} encoder failed, dropping frames: {}", codec_id, av_error_string(ret));
        self.encoder_failed = true;
        return None;
      }
      self.packet = ffi::av_packet_alloc();
      assert!(!self.packet.is_null(), "av_packet_alloc");
      self.encoder = Some(context);
      Some(context)
    }
  }

  /// Encodes `frame` and replaces out_buffer with the packets the encoder
  /// produces. Frees `frame`.
  fn encode_frame(&mut self, encoder: *mut AVCodecContext, mut frame: *mut ffi::AVFrame, quality: i32) {
    unsafe {
      (*frame).quality = quality;
      let ret = ffi::avcodec_send_frame(encoder, frame);
      assert!(ret >= 0, "avcodec_send_frame: {}", av_error_string(ret));
      ffi::av_frame_free(&mut frame);
      self.out_buffer.clear();
      loop {
        let ret = ffi::avcodec_receive_packet(encoder, self.packet);
        if ret == AVERROR_EAGAIN || ret == AVERROR_EOF {
          break;
        }
        assert!(ret >= 0, "Error during encoding: {}", av_error_string(ret));
        let slice = std::slice::from_raw_parts((*self.packet).data, (*self.packet).size as usize);
        self.out_buffer.extend_from_slice(slice);
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
      ffi::av_frame_free(&mut self.decoder_frame);
      ffi::av_packet_free(&mut self.packet_in);
      ffi::av_packet_free(&mut self.packet);
      if !self.decoded_scaler.is_null() {
        ffi::sws_freeContext(self.decoded_scaler);
      }
    }
  }
}

/// Converts images to a target format and size. It's Sync: push and pull can
/// run on different threads at once. push only takes the input lock (and the
/// frame pool's, briefly); pull holds the codec lock for its decoding and
/// encoding and takes the input and pool locks only to parse one packet or
/// take one frame. Raw input is scaled in push, encoded input decoded in
/// pull.
///
/// Locks are always taken in the order codec, input, pool. A reset from the
/// push side bumps the input generation; pull rebuilds its codec state when
/// it sees the change.
pub struct ImageConverter {
  api: ThalamusAPIThreadSafe,
  codec: Mutex<CodecState>,
  input: Mutex<InputState>,
  pool: Mutex<FramePool>,
}

impl ImageConverter {
  pub fn new(api: ThalamusAPIThreadSafe, params: ConverterParams) -> ImageConverter {
    ImageConverter {
      api,
      codec: Mutex::new(CodecState::new(0)),
      input: Mutex::new(InputState::new(params, 0)),
      pool: Mutex::new(FramePool::new(FramePoolParams::empty(), 0)),
    }
  }

  /// Starts over with `params`; pull picks it up without push waiting for it.
  pub fn reconfigure(&self, params: ConverterParams) {
    let mut input = self.input.lock().unwrap();
    self.reset(&mut input, params);
  }

  /// Replaces the input state and frame pool with ones of the next
  /// generation; pull rebuilds its codec state when it sees it.
  fn reset(&self, input: &mut InputState, params: ConverterParams) {
    let generation = input.generation + 1;
    *input = InputState::new(params, generation);
    *self.pool.lock().unwrap() = FramePool::new(FramePoolParams::empty(), generation);
  }

  /// Queues `data`'s image for conversion. Raw input is copied or scaled
  /// here; encoded input is queued for the parser.
  pub fn push(&self, data: &dyn NodeData) {
    let Some(image) = data.image() else {
      return;
    };
    let format = image.format();
    let source = ImageSource {
      format,
      pix: image_to_pix(format),
      width: image.width() as i32,
      height: image.height() as i32,
    };

    let mut guard = self.input.lock().unwrap();
    if guard.source != Some(source) {
      // A new format or size starts over with the current parameters.
      if guard.source.is_some() {
        let params = guard.params;
        self.reset(&mut guard, params);
      }
      guard.configure(source, &self.pool);
    }
    let state = &mut *guard;
    state.frame_interval = frame_interval_or_default(image.frame_interval());

    let pts = state.pts;
    state.pts_to_time.push_back((pts, data.time(), state.frame_interval));
    state.pts += 1;
    if is_compressed(format) {
      state.queue_encoded(pts, image.plane(0));
    } else {
      state.convert_raw(&self.pool, pts, image);
    }
  }

  /// Parses buffered input until one packet is ready and sends it to the
  /// decoder. Packets before the first key frame are skipped. The input lock
  /// is only held to parse: the packet is copied out first, and decoding
  /// (which avcodec_send_packet starts) runs without it. Returns false when
  /// no complete packet is buffered or the input side was reset. Only called
  /// after avcodec_receive_frame returned EAGAIN, so the decoder accepts the
  /// packet.
  fn send_next_packet(&self, codec: &mut CodecState, decoder: *mut AVCodecContext) -> bool {
    let padding = AV_INPUT_BUFFER_PADDING_SIZE as usize;
    unsafe {
      {
        let mut input = self.input.lock().unwrap();
        if input.generation != codec.generation {
          return false;
        }
        loop {
          let mut available = input.in_buffer.len().saturating_sub(padding);
          if input.params.complete_frames {
            // One message, i.e. one frame, per parse.
            available = input.slice_to_pts.front().map_or(available, |(end, _)| (*end).min(available));
          }
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
          let mut got_packet = false;
          if (*input.parser_packet).size > 0 {
            let key_frame = (*input.parser).key_frame == 1
              || (*input.parser).pict_type == ffi::AVPictureType::AV_PICTURE_TYPE_I as i32;
            if key_frame || !input.need_key_frame {
              input.need_key_frame = false;
              (*input.parser_packet).pts = (*input.parser).pts;
              let ret = ffi::av_packet_ref(codec.packet_in, input.parser_packet);
              assert!(ret >= 0, "av_packet_ref: {}", av_error_string(ret));
              got_packet = true;
            }
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
        // A corrupt packet loses its frame but shouldn't stop the stream.
        println!("ImageConverter: dropping undecodable packet: {}", av_error_string(ret));
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

      let decoded = codec.decoder_frame;
      let decoded_pix: AVPixelFormat = std::mem::transmute((*decoded).format);

      // The output depends on what the decoder produces, so it's chosen
      // here, from the first decoded frame.
      if codec.output.is_none() {
        let params = self.input.lock().unwrap().params;
        let source = codec.source.expect("decoding without a source");
        let decoded_source = ImageSource {
          format: source.format,
          pix: decoded_pix,
          width: (*decoded).width,
          height: (*decoded).height,
        };
        let output = choose_output(&params, &decoded_source);
        if (decoded_source.width, decoded_source.height, decoded_source.pix) != (output.width, output.height, output.pix) {
          codec.decoded_scaler = ffi::sws_getContext(
            decoded_source.width, decoded_source.height, decoded_source.pix,
            output.width, output.height, output.pix,
            ffi::SWS_BILINEAR,
            std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
          assert!(!codec.decoded_scaler.is_null(), "sws_getContext failed");
        }
        {
          let mut pool = self.pool.lock().unwrap();
          if pool.generation != codec.generation {
            ffi::av_frame_unref(decoded);
            return None;
          }
          *pool = FramePool::new(FramePoolParams::Video {
            format: output.pix,
            width: output.width,
            height: output.height,
          }, codec.generation);
        }
        codec.output = Some(output);
      }
      let output = codec.output.unwrap();
      if ((*decoded).width, (*decoded).height, decoded_pix) == (output.width, output.height, output.pix) {
        // Already in the output format: hand out a new reference to the
        // decoded buffers. decoder_frame is reused by the next
        // avcodec_receive_frame, so it can't be handed out itself.
        let frame = ffi::av_frame_clone(decoded);
        assert!(!frame.is_null(), "av_frame_clone failed");
        ffi::av_frame_unref(decoded);
        return Some(frame);
      }

      let frame = self.pool.lock().unwrap().get_writable(0);
      let ret = ffi::sws_scale_frame(codec.decoded_scaler, frame, decoded);
      assert!(ret >= 0, "sws_scale_frame: {}", av_error_string(ret));
      (*frame).pts = (*decoded).pts;
      ffi::av_frame_unref(decoded);

      let mut pool = self.pool.lock().unwrap();
      if !pool.push_pending(frame, codec.generation) {
        return None;
      }
      pool.get_pending(codec.generation)
    }
  }

  /// The next converted image: one per frame.
  pub fn pull(&self) -> Option<Box<dyn NodeData + '_>> {
    // Ends when pull returns, so it includes waiting for the codec lock.
    let _trace = self.api.trace_event(c"ImageConverter::pull");
    let mut codec = self.codec.lock().unwrap();

    // Catch up with the input side: a reset there starts the codec state
    // over, and raw input's output format is chosen there.
    let encoded = {
      let input = self.input.lock().unwrap();
      if codec.generation != input.generation {
        *codec = CodecState::new(input.generation);
      }
      let source = input.source?;
      let encoded = is_compressed(source.format);
      codec.source = Some(source);
      if !encoded {
        codec.output = input.output;
      }
      codec.frame_interval = input.frame_interval;
      encoded
    };

    let mut frame = if encoded {
      let _trace = self.api.trace_event(c"ImageConverter::decode");
      self.next_decoded_frame(&mut codec)?
    } else {
      self.pool.lock().unwrap().get_pending(codec.generation)?
    };
    let pts = unsafe { (*frame).pts };
    let (time, frame_interval) = self.input.lock().unwrap().take_time(pts)
      .unwrap_or_else(|| (self.api.time(), codec.frame_interval));

    let output = codec.output.expect("a converted frame without an output format");
    let Some(codec_id) = output.codec_id else {
      return Some(Box::new(RawImage {
        frame,
        num_planes: pix_to_num_planes(output.pix),
        plane_heights: pix_to_height(output.height, output.pix),
        frame_interval,
        format: output.format,
        time,
      }));
    };
    {
      let _trace = self.api.trace_event(c"ImageConverter::encode");
      let Some(encoder) = codec.open_encoder(codec_id, &output) else {
        // No encoder (logged when opening it): the frame is dropped.
        unsafe { ffi::av_frame_free(&mut frame) };
        return None;
      };
      codec.encode_frame(encoder, frame, frame_quality(codec_id, output.quality));
    }
    Some(Box::new(EncodedImage {
      codec,
      frame_interval,
      time,
      format: output.format,
      width: output.width,
      height: output.height,
    }))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn image_converter_can_be_shared_between_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ImageConverter>();
  }

  #[test]
  fn encoded_output_is_limited_to_what_the_encoder_supports() {
    let params = ConverterParams {
      format: Some(VideoFormat::Image(ImageFormat::MPEG4)),
      width: Some(641),
      height: Some(10000),
      mpeg4_quality: Some(100),
      complete_frames: false,
      h264_quality: None,
      vp9_quality: None,
    };
    let source = ImageSource {
      format: ImageFormat::NV12,
      pix: AVPixelFormat::AV_PIX_FMT_NV12,
      width: 1920,
      height: 1080,
    };
    let output = choose_output(&params, &source);
    // The MPEG-4 encoder only takes YUV420P, whose chroma is half size.
    assert_eq!(output.pix, AVPixelFormat::AV_PIX_FMT_YUV420P);
    assert_eq!((output.width, output.height), (640, 8190));
    assert_eq!(output.quality, 31);
    assert_eq!(output.codec_id, Some(ffi::AVCodecID::AV_CODEC_ID_MPEG4));
  }

  #[test]
  fn decoded_output_uses_the_decoders_pixel_format() {
    let params = ConverterParams {
      format: Some(VideoFormat::Decoded),
      width: None,
      height: None,
      mpeg4_quality: None,
      complete_frames: false,
      h264_quality: None,
      vp9_quality: None,
    };
    // Encoded input: choose_output sees the decoded frames' format.
    let decoded = ImageSource {
      format: ImageFormat::MPEG4,
      pix: AVPixelFormat::AV_PIX_FMT_YUV420P,
      width: 1920,
      height: 1080,
    };
    let output = choose_output(&params, &decoded);
    assert_eq!(output.format, ImageFormat::YUV420P);
    assert_eq!(output.pix, AVPixelFormat::AV_PIX_FMT_YUV420P);
    assert_eq!(output.codec_id, None);

    // Raw input passes through.
    let raw = ImageSource {
      format: ImageFormat::NV12,
      pix: AVPixelFormat::AV_PIX_FMT_NV12,
      width: 1920,
      height: 1080,
    };
    let output = choose_output(&params, &raw);
    assert_eq!(output.format, ImageFormat::NV12);
    assert_eq!(output.pix, AVPixelFormat::AV_PIX_FMT_NV12);
    assert_eq!(output.codec_id, None);
  }

  #[test]
  fn h264_output_is_limited_to_what_openh264_supports() {
    let params = ConverterParams {
      format: Some(VideoFormat::Image(ImageFormat::H264)),
      width: Some(8000),
      height: Some(4500),
      // Only MPEG4 uses this.
      mpeg4_quality: Some(3),
      complete_frames: false,
      h264_quality: Some(60),
      vp9_quality: None,
    };
    let source = ImageSource {
      format: ImageFormat::NV12,
      pix: AVPixelFormat::AV_PIX_FMT_NV12,
      width: 1920,
      height: 1080,
    };
    let output = choose_output(&params, &source);
    assert_eq!(output.codec_id, Some(ffi::AVCodecID::AV_CODEC_ID_H264));
    // libopenh264 only takes YUV420P.
    assert_eq!(output.pix, AVPixelFormat::AV_PIX_FMT_YUV420P);
    // Scaled down to at most 36864 macroblocks, keeping 16:9, in whole chroma
    // blocks.
    assert!(output.width as i64 * output.height as i64 <= 36864 * 256);
    assert_eq!((output.width % 2, output.height % 2), (0, 0));
    assert!((output.width as f64 / output.height as f64 - 16.0 / 9.0).abs() < 0.01);
    // Clamped to the QP range.
    assert_eq!(output.quality, 51);
  }

  #[test]
  fn vp9_output_uses_crf_in_its_own_range() {
    let params = ConverterParams {
      format: Some(VideoFormat::Image(ImageFormat::VP9)),
      width: Some(641),
      height: None,
      mpeg4_quality: Some(3),
      complete_frames: false,
      h264_quality: Some(20),
      vp9_quality: Some(70),
    };
    let source = ImageSource {
      format: ImageFormat::NV12,
      pix: AVPixelFormat::AV_PIX_FMT_NV12,
      width: 1920,
      height: 1080,
    };
    let output = choose_output(&params, &source);
    assert_eq!(output.codec_id, Some(ffi::AVCodecID::AV_CODEC_ID_VP9));
    assert_eq!(output.pix, AVPixelFormat::AV_PIX_FMT_YUV420P);
    assert_eq!((output.width, output.height), (640, 1080));
    // vp9_quality, clamped to the CRF range; quality and quantization are
    // other codecs'.
    assert_eq!(output.quality, 63);
  }

  #[test]
  fn raw_output_is_not_limited() {
    let params = ConverterParams {
      format: Some(VideoFormat::Image(ImageFormat::Gray)),
      width: Some(641),
      height: None,
      mpeg4_quality: None,
      complete_frames: false,
      h264_quality: None,
      vp9_quality: None,
    };
    let source = ImageSource {
      format: ImageFormat::NV12,
      pix: AVPixelFormat::AV_PIX_FMT_NV12,
      width: 1920,
      height: 1081,
    };
    let output = choose_output(&params, &source);
    assert_eq!(output.pix, AVPixelFormat::AV_PIX_FMT_GRAY8);
    assert_eq!((output.width, output.height), (641, 1081));
    assert_eq!(output.codec_id, None);
  }
}
