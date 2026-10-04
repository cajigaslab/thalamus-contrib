use std::collections::VecDeque;
use std::time::Duration;
use std::ffi::{CStr};

use ffmpeg_sys_next::{self as ffi, AV_INPUT_BUFFER_PADDING_SIZE, AVCodecContext, AVCodecParserContext, AVPixelFormat};

use crate::api::{ImageData, ImageFormat, NodeData, ThalamusAPIThreadSafe};

const fn mktag(a: u8, b: u8, c: u8, d: u8) -> i32 {
  (a as i32) | ((b as i32) << 8) | ((c as i32) << 16) | ((d as i32) << 24)
}

const AVERROR_EAGAIN: i32 = -ffi::EAGAIN;
pub(crate) const AVERROR_EOF: i32 = -mktag(b'E', b'O', b'F', b' ');

fn encoder_time_base(frame_interval: Duration) -> ffi::AVRational {
  unsafe { ffi::av_d2q(frame_interval.as_secs_f64(), 65535) }
}

pub struct Converter {
  api: ThalamusAPIThreadSafe,
  encoder: Option<*mut AVCodecContext>,
  pts_to_time: VecDeque<(i64, Duration)>,

  decoder: Option<*mut AVCodecContext>,
  parser: *mut AVCodecParserContext,
  decoder_frame: *mut ffi::AVFrame,

  scaler: Option<*mut ffi::SwsContext>,

  src_format: ImageFormat,
  src_width: i32,
  src_height: i32,

  dst_format: Option<ImageFormat>,
  dst_width: Option<i32>,
  dst_height: Option<i32>,
  quality: i32,

  packet: *mut ffi::AVPacket,
  parser_packet: *mut ffi::AVPacket,

  src_pix: AVPixelFormat,
  dst_pix: Option<AVPixelFormat>,

  pts: i64,

  in_buffer: Vec<u8>,
  out_buffer: Vec<u8>,

  num_input_bytes: i64,

  scaled_frame: *mut ffi::AVFrame,

  available_times: VecDeque<(Duration, Duration)>,

  initialized: bool,

  frame_interval: Duration,
  slice_to_pts: VecDeque<(usize, i64)>,

  writable_src_frames: VecDeque<*mut ffi::AVFrame>,
  pending_src_frames: VecDeque<*mut ffi::AVFrame>,
  pulled_src_frames: VecDeque<*mut ffi::AVFrame>,
  need_key_frame: bool,
}

#[derive(Debug,Clone,Copy)]
pub struct ConverterParams {
  pub format: Option<ImageFormat>,
  pub width: Option<i32>,
  pub height: Option<i32>,
  pub quality: Option<i32>,
}

pub struct EncodedImage<'a> {
  converter: &'a Converter,
  frame_interval: Duration,
  pts: Duration,
  format: ImageFormat,
  width: i32,
  height: i32,
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
    ImageFormat::MJPEG => panic!("No Pixel format"),
    ImageFormat::Gray16 => GRAY16_NATIVE,
    ImageFormat::RGB16 => RGB48_NATIVE,
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
  pub fn new(api: ThalamusAPIThreadSafe, params: ConverterParams) -> Converter {
    let ConverterParams {quality, width, height, format, ..} = params;
    let result = Converter {
      api,
      encoder: None,
      decoder: None,
      parser: std::ptr::null_mut(),
      available_times: VecDeque::new(),
      decoder_frame: std::ptr::null_mut(),
      quality: ffi::FF_QP2LAMBDA * quality.unwrap_or(5),
      dst_width: width,
      dst_height: height,
      dst_pix: format.map(image_to_pix),
      src_pix: ffi::AVPixelFormat::AV_PIX_FMT_GRAY8,
      dst_format: format,
      num_input_bytes: 0,
      in_buffer: vec![],
      scaler: None,
      src_format: ImageFormat::Gray,
      src_width: 0,
      src_height: 0,
      packet: std::ptr::null_mut(),
      parser_packet: std::ptr::null_mut(),
      pts: 0,
      out_buffer: vec![],
      scaled_frame: std::ptr::null_mut(),
      initialized: false,
      frame_interval: Duration::default(),
      pts_to_time: VecDeque::new(),
      slice_to_pts: VecDeque::new(),
      writable_src_frames: VecDeque::new(),
      pending_src_frames: VecDeque::new(),
      pulled_src_frames: VecDeque::new(),
      need_key_frame: true,
    };
    result
  }

  fn get_dst_pix(&self) -> ffi::AVPixelFormat {
    self.dst_pix.unwrap_or(self.src_pix)
  }

  fn get_dst_width(&self) -> i32 {
    self.dst_width.unwrap_or(self.src_width)
  }

  fn get_dst_height(&self) -> i32 {
    self.dst_height.unwrap_or(self.src_height)
  }

  fn get_dst_format(&self) -> ImageFormat {
    self.dst_format.unwrap_or(self.src_format)
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
        self.decoder_frame = std::ptr::null_mut();
      }
      for f in self.writable_src_frames.iter_mut() {
        ffi::av_frame_free(f);
      }
      for f in self.pending_src_frames.iter_mut() {
        ffi::av_frame_free(f);
      }
      for f in self.pulled_src_frames.iter_mut() {
        ffi::av_frame_free(f);
      }
      if self.scaled_frame != std::ptr::null_mut() {
        ffi::av_frame_free(&mut self.scaled_frame);
        self.scaled_frame = std::ptr::null_mut();
      }

      if self.packet != std::ptr::null_mut() {
        ffi::av_packet_free(&mut self.packet);
        self.packet = std::ptr::null_mut();
      }
      if self.parser_packet != std::ptr::null_mut() {
        ffi::av_packet_free(&mut self.parser_packet);
        self.parser_packet = std::ptr::null_mut();
      }

      let src_pix = image_to_pix(src_format);
      
      self.src_format = src_format;
      self.src_pix = src_pix;
      self.src_width = src_width;
      self.src_height = src_height;
      let time_base = encoder_time_base(frame_interval);

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

        self.decoder_frame = ffi::av_frame_alloc();
        (*self.decoder_frame).format = src_pix as i32;
        (*self.decoder_frame).width = src_width;
        (*self.decoder_frame).height = src_height;
      }

      self.writable_src_frames.clear();
      self.pending_src_frames.clear();
      self.pulled_src_frames.clear();

      let dst_pix = self.get_dst_pix();
      let dst_width = self.get_dst_width();
      let dst_height = self.get_dst_height();

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
      self.need_key_frame = true;
      self.available_times.clear();
      self.pts_to_time.clear();

      if is_compressed(self.get_dst_format()) {
        let quality = self.quality;

        let codec_id = format_codec(self.get_dst_format());
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

  pub fn needs_conversion(&self, data: &dyn NodeData) -> bool {
    let Some(input) = data.image() else {
      return false;
    };
    let input_format = (input.format(), input.width() as i32, input.height() as i32);
    let target_format = (
      self.dst_format.unwrap_or(input_format.0),
      self.dst_width.unwrap_or(input_format.1),
      self.dst_height.unwrap_or(input_format.2));
    input_format != target_format
  }

  fn get_writable_src_frame(&mut self) -> *mut ffi::AVFrame {
    unsafe {
      self.pulled_src_frames.retain(|f| {
        let writable = ffi::av_frame_is_writable(*f) != 0;
        if writable {
          self.writable_src_frames.push_back(*f);
        }
        !writable
      });

      if !self.writable_src_frames.is_empty() {
        return self.writable_src_frames.pop_front().unwrap();
      }

      let frame = ffi::av_frame_alloc();
      (*frame).format = self.src_pix as i32;
      (*frame).width = self.src_width;
      (*frame).height = self.src_height;
      let ret = ffi::av_frame_get_buffer(frame, 0);
      assert!(ret >= 0, "ffi::av_frame_get_buffer: {}", av_error_string(ret));
      frame
    }
  }

  fn push_pending_src_frame(&mut self, frame: *mut ffi::AVFrame) {
    self.pending_src_frames.push_back(frame);
  }

  fn get_pending_src_frame(&mut self) -> Option<*mut ffi::AVFrame> {
    let result = self.pending_src_frames.pop_front();
    if let Some(frame) = result {
      self.pulled_src_frames.push_back(frame);
    }
    result
  }

  pub fn push(&mut self, data: &dyn NodeData) {
    let Some(input) = data.image() else {
      return;
    };

    let current_format = (self.src_format, self.src_width, self.src_height);
    let input_format = (input.format(), input.width() as i32, input.height() as i32);

    let raw_frame_interval = input.frame_interval();
    self.frame_interval = if raw_frame_interval.as_nanos() == 0 { Duration::from_millis(16) } else { raw_frame_interval };

    if !self.initialized || current_format != input_format {
      self.configure(input.width() as i32, input.height() as i32, input.format(), self.frame_interval);
      self.initialized = true;
    }


    //self.frame_interval = input.frame_interval();
    //self.available_times.push_back((pts, input.frame_interval()));
    let pts = self.pts;
    self.pts_to_time.push_back((pts, data.time()));
    self.pts += 1;
    unsafe {
      if let Some(_) = self.decoder {
        let plane = input.plane(0);

        let buffer_pos = self.in_buffer.len() - AV_INPUT_BUFFER_PADDING_SIZE as usize;
        self.in_buffer.resize(self.in_buffer.len() + plane.len(), 0);

        let end = buffer_pos+plane.len();
        self.in_buffer[buffer_pos..end].copy_from_slice(plane);
        self.slice_to_pts.push_back((end, pts));
        
      } else {
        let frame = self.get_writable_src_frame();
        node_to_image(input, (*frame).data, (*frame).linesize);
        (*frame).pts = pts;
        self.push_pending_src_frame(frame);
      }
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
          let discard = self.need_key_frame && (*self.parser).pict_type != ffi::AVPictureType::AV_PICTURE_TYPE_I as i32;
          if !discard {
            self.need_key_frame = false;
            (*self.parser_packet).pts = (*self.parser).pts;
            let ret = ffi::avcodec_send_packet(decoder, self.parser_packet);
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

  pub fn reconfigure(&mut self, params: ConverterParams) {
    let ConverterParams {quality, width, height, format, ..} = params;
    self.quality = ffi::FF_QP2LAMBDA * quality.unwrap_or(5);
    self.dst_pix = format.map(image_to_pix);
    self.dst_width = width;
    self.dst_height = height;
    self.dst_format = format;
    self.initialized = false;
  }

  pub fn pull<'a>(&'a mut self) -> Option<Box<dyn NodeData + 'a>> {
    unsafe {
      let (decoded_frame, need_unref) = if let Some(decoder) = self.decoder {
        self.parser_to_decoder(decoder);

        let ret = ffi::avcodec_receive_frame(decoder, self.decoder_frame);
        if ret == AVERROR_EAGAIN || ret == AVERROR_EOF {
          return None;                   // needs more input / fully drained
        }
        assert!(ret >= 0, "avcodec_receive_frame: {}", av_error_string(ret));
        (self.decoder_frame, true)
      } else {
        match self.get_pending_src_frame() {
          Some(f) => { (f, false) }
          None => {return None;}
        }
      };

      let pts = match self.pts_to_time.iter().position(|(pts, _)| pts == &(*decoded_frame).pts) {
        Some(i) => {
          let temp = self.pts_to_time[i];
          self.pts_to_time.remove(i);
          temp.1
        },
        None => self.api.time()
      };

      //let pts = Duration::from_nanos((*decoded_frame).pts as u64);
      let frame_interval = self.frame_interval;
      //let (_, frame_interval) = self.available_times.pop_front().expect("No pts for decoded frame");

      let (scaled_frame, need_unref2) = if let Some(scaler) = self.scaler {
        let ret = ffi::av_frame_make_writable(self.scaled_frame);
        assert!(ret >= 0, "av_frame_make_writable failed: {}", av_error_string(ret));
        let ret = ffi::sws_scale_frame(scaler, self.scaled_frame, decoded_frame);
        assert!(ret >= 0, "sws_scale_frame: {}", av_error_string(ret));
        if need_unref {
          ffi::av_frame_unref(decoded_frame);
        }
        (self.scaled_frame, false)
      } else {
        (decoded_frame, need_unref)
      };

      if let Some(encoder) = self.encoder {
        (*scaled_frame).quality = self.quality;
        let ret = ffi::avcodec_send_frame(encoder, scaled_frame);
        assert!(ret >= 0, "avcodec_send_frame: {}", av_error_string(ret));
        if need_unref2 {
          ffi::av_frame_unref(scaled_frame);
        }
        self.out_buffer.clear();
        loop {
          let ret = ffi::avcodec_receive_packet(encoder, self.packet);
          if ret == AVERROR_EAGAIN || ret == AVERROR_EOF {
            break;
          }
          assert!(ret >= 0, "Error during encoding: {}", av_error_string(ret));

          let slice = std::slice::from_raw_parts((*self.packet).data, (*self.packet).size as usize);
          self.out_buffer.extend_from_slice(slice);
        }

        let width = self.get_dst_width();
        let height = self.get_dst_height();
        let format = self.get_dst_format();
        return Some(Box::new(EncodedImage {
          converter: self, frame_interval, pts, width, height, format,
        }));
      } else {

        let num_planes = pix_to_num_planes(self.get_dst_pix());
        let plane_heights = pix_to_height(self.get_dst_height(), self.get_dst_pix());
        return Some(Box::new(RawImage {
          frame: scaled_frame, num_planes, plane_heights, frame_interval, format: self.get_dst_format(), pts, need_unref: need_unref2
        }));
      };
    }
  }

  //fn new(params: ConverterParams) -> Converter {}
    
}

impl Drop for Converter {
  fn drop(&mut self) {
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
      for f in self.writable_src_frames.iter_mut() {
        ffi::av_frame_free(f);
      }
      for f in self.pending_src_frames.iter_mut() {
        ffi::av_frame_free(f);
      }
      for f in self.pulled_src_frames.iter_mut() {
        ffi::av_frame_free(f);
      }
      if self.scaled_frame != std::ptr::null_mut() {
        ffi::av_frame_free(&mut self.scaled_frame);
      }

      if self.packet != std::ptr::null_mut() {
        ffi::av_packet_free(&mut self.packet);
      }
      if self.parser_packet != std::ptr::null_mut() {
        ffi::av_packet_free(&mut self.parser_packet);
      }
    }
  }
}
