use std::collections::VecDeque;
use std::time::Duration;
use std::ffi::{CStr};

use ffmpeg_sys_next::{self as ffi, AV_INPUT_BUFFER_PADDING_SIZE, AVCodecContext, AVCodecParserContext, AVPixelFormat};

use crate::api::{ImageData, ImageFormat, NodeData};

const fn mktag(a: u8, b: u8, c: u8, d: u8) -> i32 {
  (a as i32) | ((b as i32) << 8) | ((c as i32) << 16) | ((d as i32) << 24)
}

const AVERROR_EAGAIN: i32 = -ffi::EAGAIN;
pub(crate) const AVERROR_EOF: i32 = -mktag(b'E', b'O', b'F', b' ');

const NANOSECOND_TIME_BASE: ffi::AVRational = ffi::AVRational {
  num: 1,
  den: 1_000_000_000,
};

fn encoder_time_base(frame_interval: Duration) -> ffi::AVRational {
  unsafe { ffi::av_d2q(frame_interval.as_secs_f64(), 65535) }
}

pub struct Converter {
  encoder: Option<*mut AVCodecContext>,

  decoder: Option<*mut AVCodecContext>,
  parser: *mut AVCodecParserContext,
  decoder_frame: *mut ffi::AVFrame,

  scaler: Option<*mut ffi::SwsContext>,

  src_format: ImageFormat,
  src_width: i32,
  src_height: i32,

  dst_format: ImageFormat,
  dst_width: i32,
  dst_height: i32,
  quality: i32,

  packet: *mut ffi::AVPacket,
  parser_packet: *mut ffi::AVPacket,

  src_pix: AVPixelFormat,
  dst_pix: AVPixelFormat,

  pts: i64,

  in_buffer: Vec<u8>,
  out_buffer: Vec<u8>,

  num_input_bytes: i64,

  scaled_frame: *mut ffi::AVFrame,

  available_times: VecDeque<(Duration, Duration)>,

  src_frame: *mut ffi::AVFrame,
}

pub struct ConverterParams {
  pub format: ImageFormat,
  pub width: i32,
  pub height: i32,
  pub quality: i32,
  pub frame_interval: Duration,
}

pub struct EncodedImage<'a> {
  converter: &'a Converter,
  frame_interval: Duration,
  pts: Duration,
}

impl<'a> NodeData for EncodedImage<'a> {
  fn time(&self) -> Duration {
    self.pts
  }

  fn image(&self) -> Option<&dyn ImageData> {
    Some(self)
  }
}

impl<'a> ImageData for EncodedImage<'a> {
  fn plane(&self, _channel: i32) -> &[u8] {
    &self.converter.out_buffer
  }

  fn num_planes(&self) -> u64 {
    1
  }

  fn format(&self) -> ImageFormat {
    self.converter.dst_format
  }

  fn width(&self) -> u64 {
    self.converter.dst_width as u64
  }

  fn height(&self) -> u64 {
    self.converter.dst_height as u64
  }

  fn frame_interval(&self) -> Duration {
    self.frame_interval
  }
}

struct RawImage {
  frame: *mut ffi::AVFrame,
  num_planes: i32,
  format: ImageFormat,
  plane_heights: [i32; 4],
  frame_interval: Duration,
  pts: Duration,
  need_unref: bool
}

impl<'a> NodeData for RawImage {
  fn time(&self) -> Duration {
    self.pts
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
    if self.need_unref {
      unsafe { ffi::av_frame_unref(self.frame) };
    }
  }
}

struct PassthroughImage<'a> {
  underlying: &'a dyn ImageData,
  pts: Duration,
}

impl<'a> NodeData for PassthroughImage<'a> {
  fn time(&self) -> Duration {
    self.pts
  }

  fn image(&self) -> Option<&dyn ImageData> {
    Some(self)
  }
}

impl<'a> ImageData for PassthroughImage<'a> {
  fn plane(&self, i: i32) -> &[u8] {
    self.underlying.plane(i)
  }

  fn num_planes(&self) -> u64 {
    self.underlying.num_planes()
  }

  fn format(&self) -> ImageFormat {
    self.underlying.format()
  }

  fn width(&self) -> u64 {
    self.underlying.width()
  }

  fn height(&self) -> u64 {
    self.underlying.height()
  }

  fn frame_interval(&self) -> Duration {
    self.underlying.frame_interval()
  }
}

fn is_compressed(format: ImageFormat) -> bool {
  format == ImageFormat::MPEG4
}

fn format_codec(format: ImageFormat) -> ffi::AVCodecID {
  match format {
    ImageFormat::MPEG4 => ffi::AVCodecID::AV_CODEC_ID_MPEG4,
    _ => panic!("Unsupported format")
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

fn pix_to_height(height: i32, pix: AVPixelFormat) -> [i32; 4] {
  match pix {
    AVPixelFormat::AV_PIX_FMT_GRAY8 => [height, 0, 0, 0],
    AVPixelFormat::AV_PIX_FMT_RGB24 => [height, 0, 0, 0],
    AVPixelFormat::AV_PIX_FMT_YUYV422 => [height, 0, 0, 0],
    AVPixelFormat::AV_PIX_FMT_YUV420P => [height, height/2, height/2, 0],
    AVPixelFormat::AV_PIX_FMT_YUVJ420P => [height, height/2, height/2, 0],
    AVPixelFormat::AV_PIX_FMT_NV12 => [height*3/2, 0, 0, 0],
    AVPixelFormat::AV_PIX_FMT_BGR24 => [height, 0, 0, 0],
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
    AVPixelFormat::AV_PIX_FMT_NV12 => 1,
    AVPixelFormat::AV_PIX_FMT_BGR24 => 1,
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
    ImageFormat::MJPEG => panic!("No Pixel format"),
  }
}

fn node_to_image(input: &dyn ImageData, data: [*mut u8; 8], linesize: [i32; 8]) {
  let format = input.format();
  let pix = image_to_pix(format);
  let input_heights = pix_to_height(input.height() as i32, pix);
  for i in 0..(input.num_planes() as usize) {
    let plane = input.plane(i as i32);
    let height = input_heights[i] as i32;
    let in_linesize = (plane.len() as i32)/height;
    let out_linesize = linesize[i];
    let out_plane = unsafe { std::slice::from_raw_parts_mut(data[i], (out_linesize*height) as usize) };

    if in_linesize == out_linesize {
      out_plane.copy_from_slice(plane);
    } else {
      let min_linesize = in_linesize.min(out_linesize);
      for y in 0..height {
        let in_index = ((y*in_linesize) as usize)..((y*in_linesize + min_linesize) as usize);
        let out_index = ((y*out_linesize) as usize)..((y*out_linesize + min_linesize) as usize);
        out_plane[out_index].copy_from_slice(&plane[in_index]);
      }
    }
  }
}

unsafe impl Send for Converter {}

impl Converter {
  pub fn new(params: ConverterParams) -> Converter {
    let ConverterParams {quality, width, height, format, frame_interval, ..} = params;
    let mut result = Converter {
      encoder: None,
      decoder: None,
      parser: std::ptr::null_mut(),
      available_times: VecDeque::new(),
      decoder_frame: std::ptr::null_mut(),
      quality: ffi::FF_QP2LAMBDA * quality,
      dst_width: width,
      dst_height: height,
      dst_pix: image_to_pix(format),
      src_pix: image_to_pix(format),
      dst_format: format,
      num_input_bytes: 0,
      in_buffer: vec![],
      scaler: Some(std::ptr::null_mut()),
      src_format: format,
      src_width: width,
      src_height: height,
      packet: std::ptr::null_mut(),
      parser_packet: std::ptr::null_mut(),
      pts: 0,
      out_buffer: vec![],
      scaled_frame: std::ptr::null_mut(),
      src_frame: std::ptr::null_mut(),
    };
    result.configure(width, height, format, frame_interval);
    result
  }

  fn configure(&mut self, src_width: i32, src_height: i32, src_format: ImageFormat, frame_interval: Duration) {
    unsafe {
      if let Some(scaler) = self.scaler {
        ffi::sws_freeContext(scaler);
        self.scaler = None;
      }
      if let Some(mut decoder) = self.decoder {
        ffi::avcodec_free_context(&mut decoder);
        ffi::av_parser_close(self.parser);
        self.decoder = None;
      }
      if let Some(mut encoder) = self.encoder {
        ffi::avcodec_free_context(&mut encoder);
        self.encoder = None;
      }

      if self.decoder_frame != std::ptr::null_mut() {
        ffi::av_frame_free(&mut self.decoder_frame);
      }
      if self.src_frame != std::ptr::null_mut() {
        ffi::av_frame_free(&mut self.src_frame);
      }

      if self.packet != std::ptr::null_mut() {
        ffi::av_packet_free(&mut self.packet);
      }
      if self.parser_packet != std::ptr::null_mut() {
        ffi::av_packet_free(&mut self.parser_packet);
      }

      let src_pix = image_to_pix(src_format);
      
      self.src_format = src_format;
      self.src_pix = src_pix;
      self.src_width = src_width;
      self.src_height = src_height;

      if is_compressed(src_format) {
        let codec_id = format_codec(src_format);
        let codec = ffi::avcodec_find_decoder(codec_id);
        if codec.is_null() {
          panic!("no {:?} decoder in this FFmpeg build", codec_id);
        }

        let mut context = ffi::avcodec_alloc_context3(codec);
        if context.is_null() {
          panic!("avcodec_alloc_context3 failed");
        }

        (*context).pkt_timebase = NANOSECOND_TIME_BASE;
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

        self.decoder_frame = ffi::av_frame_alloc();
        (*self.decoder_frame).format = src_pix as i32;
        (*self.decoder_frame).width = src_width;
        (*self.decoder_frame).height = src_height;
      }

      self.src_frame = ffi::av_frame_alloc();
      (*self.src_frame).format = src_pix as i32;
      (*self.src_frame).width = src_width;
      (*self.src_frame).height = src_height;
      let ret = ffi::av_frame_get_buffer(self.src_frame, 0);
      assert!(ret >= 0, "ffi::av_frame_get_buffer: {}", av_error_string(ret));

      let dst_pix = self.dst_pix;
      let dst_width = self.dst_width;
      let dst_height = self.dst_height;

      if (src_width, src_height, src_pix) != (dst_width, dst_height, dst_pix) {
        let scaler = ffi::sws_getContext(
            src_width, src_height, src_pix, 
            dst_width, dst_height, dst_pix, 
            ffi::SWS_BILINEAR,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut());
        self.scaler = Some(scaler);
      } else {
        self.scaler = None;
      }

      self.packet = ffi::av_packet_alloc();
      self.parser_packet = ffi::av_packet_alloc();

      self.scaled_frame = ffi::av_frame_alloc();
      (*self.scaled_frame).format = dst_pix as i32;
      (*self.scaled_frame).width = dst_width;
      (*self.scaled_frame).height = dst_height;
      let ret = ffi::av_frame_get_buffer(self.scaled_frame, 0);
      assert!(ret >= 0, "ffi::av_frame_get_buffer: {}", av_error_string(ret));

      self.pts = 0;
      self.num_input_bytes = 0;
      self.available_times.clear();

      if is_compressed(self.dst_format) {
        let quality = self.quality;
        let time_base = encoder_time_base(frame_interval);

        let codec_id = format_codec(self.dst_format);
        let codec = ffi::avcodec_find_encoder(codec_id);
        if codec.is_null() {
          panic!("no {:?} encoder in this FFmpeg build", codec_id);
        }

        let mut context = ffi::avcodec_alloc_context3(codec);
        if context.is_null() {
          panic!("avcodec_alloc_context3 failed");
        }

        (*context).width = dst_width;
        (*context).height = dst_height;
        (*context).pix_fmt = dst_pix;
        (*context).time_base = time_base;
        (*context).framerate = ffi::AVRational {
          num: time_base.den,
          den: time_base.num,
        };
        (*context).gop_size = time_base.den/time_base.num;
        (*context).flags |= ffi::AV_CODEC_FLAG_QSCALE as i32;
        (*context).global_quality = quality;
        (*context).max_b_frames = 0;

        let ret = ffi::avcodec_open2(context, codec, std::ptr::null_mut());
        if ret < 0 {
          ffi::avcodec_free_context(&mut context);
          panic!("opening {:?} encoder failed: {}", codec_id, av_error_string(ret));
        }

        self.encoder = Some(context);
      }
    }
  }

  pub fn push<'a, 'b>(&'a mut self, data: &'b dyn NodeData) -> ConverterResult<'a, 'b> {
    let Some(input) = data.image() else {
      return ConverterResult { converter: self, passthrough: None, exhausted: true };
    };
    let pts = data.time();

    let current_format = (self.src_format, self.src_width, self.src_height);
    let input_format = (input.format(), input.width() as i32, input.height() as i32);
    if current_format != input_format {
      self.configure(input.width() as i32, input.height() as i32, input.format(), input.frame_interval());
    }

    if input_format == (self.dst_format, self.dst_width, self.dst_height) {
      let passthrough = Some(Box::new(PassthroughImage{ underlying: input, pts} ));
      return ConverterResult { converter: self, passthrough, exhausted: false };
    }

    self.available_times.push_back((pts, input.frame_interval()));
    let pts = pts.as_nanos() as i64;
    unsafe {
      if let Some(decoder) = self.decoder {
        let plane = input.plane(0);

        let buffer_pos = self.in_buffer.len() - AV_INPUT_BUFFER_PADDING_SIZE as usize;
        self.in_buffer.resize(self.in_buffer.len() + plane.len(), 0);
        self.in_buffer[buffer_pos..(buffer_pos+plane.len())].copy_from_slice(plane);

        let mut offset = 0;
        loop {
          let used = ffi::av_parser_parse2(
            self.parser, decoder, 
            &mut (*self.parser_packet).data,
            &mut (*self.parser_packet).size,
            self.in_buffer.as_ptr().add(offset),
            (self.in_buffer.len() - (AV_INPUT_BUFFER_PADDING_SIZE as usize) - offset) as i32,
            pts, ffi::AV_NOPTS_VALUE, 
            self.num_input_bytes
          );
          if used == 0 {
            break;
          }
          offset += used as usize;
          self.num_input_bytes += used as i64;

          let ret = ffi::avcodec_send_packet(decoder, self.parser_packet);
          assert!(ret >= 0, "avcodec_send_packet {}", av_error_string(ret));
        }
        if offset > 0 {
          self.in_buffer.drain(0..offset);
        }
      } else { 
        node_to_image(input, (*self.src_frame).data, (*self.src_frame).linesize);
      }
    }

    ConverterResult { converter: self, passthrough: None, exhausted: false }
  }

  fn pull<'a>(&'a mut self) -> Option<Box<dyn NodeData + 'a>> {
    unsafe {
      let (decoded_frame, need_unref) = if let Some(decoder) = self.decoder {
        let ret = ffi::avcodec_receive_frame(decoder, self.decoder_frame);
        if ret == AVERROR_EAGAIN || ret == AVERROR_EOF {
          return None;                   // needs more input / fully drained
        }
        assert!(ret >= 0, "avcodec_receive_frame: {}", av_error_string(ret));
        (self.decoder_frame, true)
      } else {
        (self.src_frame, false)
      };

      let (pts, frame_interval) = self.available_times.pop_front().expect("No pts for decoded frame");

      let (scaled_frame, need_unref2) = if let Some(scaler) = self.scaler {
        let ret = ffi::av_frame_make_writable(self.scaled_frame);
        assert!(ret >= 0, "av_frame_make_writable failed");
        ffi::sws_scale_frame(scaler, self.scaled_frame, decoded_frame);
        if need_unref {
          ffi::av_frame_unref(decoded_frame);
        }
        (self.scaled_frame, false)
      } else {
        (decoded_frame, true)
      };

      if let Some(encoder) = self.encoder {
        ffi::avcodec_send_frame(encoder, scaled_frame);
        if need_unref {
          ffi::av_frame_unref(scaled_frame);
        }
        self.out_buffer.clear();
        loop {
          let ret = ffi::avcodec_receive_packet(encoder, self.packet);
          if ret == AVERROR_EAGAIN || ret == AVERROR_EOF {
            break;
          }
          assert!(ret >= 0, "Error during encoding");

          let slice = std::slice::from_raw_parts((*self.packet).data, (*self.packet).size as usize);
          self.out_buffer.extend_from_slice(slice);
        }

        return Some(Box::new(EncodedImage {
          converter: self, frame_interval, pts
        }));
      } else {

        let num_planes = pix_to_num_planes(self.dst_pix);
        let plane_heights = pix_to_height(self.dst_height, self.dst_pix);
        return Some(Box::new(RawImage {
          frame: scaled_frame, num_planes, plane_heights, frame_interval, format: self.dst_format, pts, need_unref: need_unref2
        }));
      };
    }
  }

  //fn new(params: ConverterParams) -> Converter {}
    
}

pub struct ConverterResult<'a, 'b> {
  converter: &'a mut Converter,
  passthrough: Option<Box<PassthroughImage<'b>>>,
  exhausted: bool,
}

impl<'a, 'b: 'a> ConverterResult<'a, 'b> {
  pub fn pull<'c>(&'c mut self) -> Option<Box<dyn NodeData + 'c>> {
    if self.exhausted {
      return None;
    }
    match self.passthrough.take() {
      Some(p) => {
        self.exhausted = true;
        return Some(p)
      },
      None => {}
    };

    let temp = self.converter.pull();
    self.exhausted = temp.is_none();
    temp
  }
}
