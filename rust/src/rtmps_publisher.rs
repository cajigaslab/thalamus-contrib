use std::ffi::{CStr, CString};
use std::sync::OnceLock;
use std::time::Duration;

use ffmpeg_sys_next as ffi;
use skia_safe::{AlphaType, Color, ColorType, Font, FontMgr, ImageInfo, Paint, Point, Rect, surfaces};

use crate::api::ImageFormat;

// Bundled directly rather than relying on Font::default()'s system font
// manager: FontMgr::default() finds the system's real font families fine,
// but Font::default()'s typeface doesn't go through it and resolves to an
// empty, zero-glyph stub here -- silently rendering no text at all instead
// of erroring. See assets/DejaVuSans-LICENSE.txt.
static OVERLAY_FONT_DATA: &[u8] = include_bytes!("../assets/DejaVuSans.ttf");

fn overlay_font() -> &'static Font {
  static FONT: OnceLock<Font> = OnceLock::new();
  FONT.get_or_init(|| {
    let typeface = FontMgr::new()
      .new_from_data(OVERLAY_FONT_DATA, None)
      .expect("bundled DejaVuSans.ttf should parse as a valid font");
    Font::from_typeface(typeface, OVERLAY_FONT_SIZE)
  })
}

// One decoded frame, copied out of a NodeData's borrowed planes so it can
// cross a thread boundary to the publisher thread (see rtmps_node.rs).
pub struct FrameBuf {
  pub format: ImageFormat,
  pub width: u32,
  pub height: u32,
  pub planes: Vec<Vec<u8>>,
  pub timestamp: Duration,
  pub frame_interval: Duration,
}

fn av_pixel_format(format: ImageFormat) -> ffi::AVPixelFormat {
  match format {
    ImageFormat::Gray => ffi::AVPixelFormat::AV_PIX_FMT_GRAY8,
    ImageFormat::RGB => ffi::AVPixelFormat::AV_PIX_FMT_RGB24,
    ImageFormat::YUYV422 => ffi::AVPixelFormat::AV_PIX_FMT_YUYV422,
    ImageFormat::YUV420P => ffi::AVPixelFormat::AV_PIX_FMT_YUV420P,
    ImageFormat::YUVJ420P => ffi::AVPixelFormat::AV_PIX_FMT_YUVJ420P,
  }
}

// Matches the tightly-packed (no linesize padding) plane layout used by
// every NodeData image producer in this codebase (see plane_layout in
// ffmpeg_devices.rs).
fn plane_linesizes(format: ImageFormat, width: u32) -> Vec<i32> {
  match format {
    ImageFormat::Gray => vec![width as i32],
    ImageFormat::RGB => vec![width as i32 * 3],
    ImageFormat::YUYV422 => vec![width as i32 * 2],
    ImageFormat::YUV420P | ImageFormat::YUVJ420P => {
      let chroma_w = width.div_ceil(2) as i32;
      vec![width as i32, chroma_w, chroma_w]
    }
  }
}

// AVCodecContext.time_base doubles as a nominal-frame-rate hint: h264_mf.c
// (the encoder used here, see open()) derives the MF_MT_FRAME_RATE attribute
// it hands to the underlying Media Foundation encoder from it. A time_base
// that doesn't correspond to a real frame rate (e.g. an arbitrary fine clock
// tick) makes the MFT reject SetOutputType outright, so this has to reflect
// the source's actual frame interval, not just be precise.
fn duration_to_time_base(interval: Duration) -> ffi::AVRational {
  if interval.is_zero() {
    return ffi::AVRational { num: 1, den: 30 };
  }
  unsafe { ffi::av_d2q(interval.as_secs_f64(), 1_000_000) }
}

const OVERLAY_MARGIN: i32 = 10;
const OVERLAY_FONT_SIZE: f32 = 24.0;
const OVERLAY_PADDING: i32 = 8;

// Draws `text` directly into the frame's Y (luma) plane: white text on a
// translucent dark box, bottom-left corner. Wraps the plane's own bytes as
// a Gray8 Skia surface (row bytes = the AVFrame's real linesize, which can
// exceed width due to alignment padding) rather than rendering to a
// separate buffer and compositing by hand -- Skia's normal SrcOver
// blending already alpha-blends each draw against whatever's underneath,
// which for a Gray8 destination *is* the actual video luma. Chroma is left
// untouched, so the overlay reads as grayscale, a fine tradeoff for a small
// corner timestamp.
fn draw_overlay_text(frame: *mut ffi::AVFrame, text: &str) {
  let frame_w = unsafe { (*frame).width };
  let frame_h = unsafe { (*frame).height };
  let linesize = unsafe { (*frame).linesize[0] };
  let y_data = unsafe { (*frame).data[0] };
  if frame_w <= 0 || frame_h <= 0 || linesize <= 0 || y_data.is_null() {
    return;
  }

  let y_len = linesize as usize * frame_h as usize;
  let y_plane = unsafe { std::slice::from_raw_parts_mut(y_data, y_len) };

  let info = ImageInfo::new((frame_w, frame_h), ColorType::Gray8, AlphaType::Opaque, None);
  let Some(mut surface) = surfaces::wrap_pixels(&info, y_plane, linesize as usize, None) else {
    return;
  };
  let canvas = surface.canvas();

  let font = overlay_font();
  let mut text_paint = Paint::default();
  text_paint.set_anti_alias(true);
  text_paint.set_color(Color::WHITE);

  let (advance, bounds) = font.measure_str(text, Some(&text_paint));
  let box_w = (advance.ceil() as i32 + OVERLAY_PADDING * 2).min(frame_w);
  let box_h = (bounds.height().ceil() as i32 + OVERLAY_PADDING * 2).min(frame_h);
  if box_w <= 0 || box_h <= 0 {
    return;
  }

  let box_x = OVERLAY_MARGIN as f32;
  let box_y = (frame_h - OVERLAY_MARGIN - box_h) as f32;

  let mut bg_paint = Paint::default();
  bg_paint.set_anti_alias(true);
  bg_paint.set_color(Color::from_argb(160, 0, 0, 0));
  canvas.draw_rect(
    Rect::from_xywh(box_x, box_y, box_w as f32, box_h as f32),
    &bg_paint,
  );

  // bounds.top is negative (the ascent, measured above the baseline), so
  // this places the glyphs' top edge OVERLAY_PADDING below the box's top.
  let baseline_y = box_y + OVERLAY_PADDING as f32 - bounds.top;
  canvas.draw_str(
    text,
    Point::new(box_x + OVERLAY_PADDING as f32, baseline_y),
    font,
    &text_paint,
  );
}

const fn mktag(a: u8, b: u8, c: u8, d: u8) -> i32 {
  (a as i32) | ((b as i32) << 8) | ((c as i32) << 16) | ((d as i32) << 24)
}
// bindgen doesn't expand these two function-like macros from error.h, so
// they're reproduced here from libavutil/error.h's definitions.
const AVERROR_EAGAIN: i32 = -ffi::EAGAIN;
const AVERROR_EOF: i32 = -mktag(b'E', b'O', b'F', b' ');

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

// Encodes incoming frames to H.264 and muxes them into an FLV stream
// published to an RTMP(S) destination. Owns the whole libavformat/libavcodec
// pipeline; not constructible/usable without an actual frame size, since the
// encoder has to be configured with fixed dimensions up front.
pub struct RtmpsPublisher {
  fmt_ctx: *mut ffi::AVFormatContext,
  codec_ctx: *mut ffi::AVCodecContext,
  sws_ctx: *mut ffi::SwsContext,
  frame: *mut ffi::AVFrame,
  packet: *mut ffi::AVPacket,
  stream_time_base: ffi::AVRational,
  encoder_time_base: ffi::AVRational,
  width: u32,
  height: u32,
  src_format: ImageFormat,
  first_timestamp: Option<Duration>,
  opened_pb: bool,
}

// Every field is a raw pointer into memory this struct exclusively owns and
// only ever touches from whichever single thread currently holds &mut self;
// none of it is thread-affine (no COM/TLS-style thread ownership the way the
// dshow bits in ffmpeg_devices.rs are), so moving the whole publisher to a
// dedicated worker thread once is sound.
unsafe impl Send for RtmpsPublisher {}

impl RtmpsPublisher {
  pub fn open(
    destination: &str,
    format: ImageFormat,
    width: u32,
    height: u32,
    frame_interval: Duration,
  ) -> Result<Self, String> {
    if width == 0 || height == 0 {
      return Err("frame has no dimensions".to_string());
    }

    unsafe { ffi::avformat_network_init() };

    let dest_cstr = CString::new(destination).map_err(|e| e.to_string())?;
    let flv_cstr = CString::new("flv").unwrap();

    let mut fmt_ctx: *mut ffi::AVFormatContext = std::ptr::null_mut();
    let ret = unsafe {
      ffi::avformat_alloc_output_context2(
        &mut fmt_ctx,
        std::ptr::null_mut(),
        flv_cstr.as_ptr(),
        dest_cstr.as_ptr(),
      )
    };
    if ret < 0 || fmt_ctx.is_null() {
      return Err(format!(
        "avformat_alloc_output_context2 failed: {}",
        av_error_string(ret)
      ));
    }

    let codec = unsafe { ffi::avcodec_find_encoder(ffi::AVCodecID::AV_CODEC_ID_H264) };
    if codec.is_null() {
      unsafe { ffi::avformat_free_context(fmt_ctx) };
      return Err("no H.264 encoder available in this FFmpeg build".to_string());
    }

    let stream = unsafe { ffi::avformat_new_stream(fmt_ctx, std::ptr::null()) };
    if stream.is_null() {
      unsafe { ffi::avformat_free_context(fmt_ctx) };
      return Err("avformat_new_stream failed".to_string());
    }

    let mut codec_ctx = unsafe { ffi::avcodec_alloc_context3(codec) };
    if codec_ctx.is_null() {
      unsafe { ffi::avformat_free_context(fmt_ctx) };
      return Err("avcodec_alloc_context3 failed".to_string());
    }

    // Each frame's pts is still derived from its own NodeData timestamp (see
    // write_frame) rather than assuming a perfectly regular clock, so
    // jitter in the source's actual frame interval doesn't drift the
    // stream -- this time_base is just the nominal rate advertised to the
    // encoder (see duration_to_time_base).
    let encoder_time_base = duration_to_time_base(frame_interval);
    unsafe {
      (*codec_ctx).width = width as i32;
      (*codec_ctx).height = height as i32;
      (*codec_ctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P;
      (*codec_ctx).time_base = encoder_time_base;
      (*codec_ctx).gop_size = 60;
      (*codec_ctx).max_b_frames = 0;
      (*codec_ctx).bit_rate = 4_000_000;
      if (*(*fmt_ctx).oformat).flags & ffi::AVFMT_GLOBALHEADER != 0 {
        (*codec_ctx).flags |= ffi::AV_CODEC_FLAG_GLOBAL_HEADER as i32;
      }
    }

    let ret = unsafe { ffi::avcodec_open2(codec_ctx, codec, std::ptr::null_mut()) };
    if ret < 0 {
      let msg = format!("avcodec_open2 failed: {}", av_error_string(ret));
      unsafe {
        ffi::avcodec_free_context(&mut codec_ctx);
        ffi::avformat_free_context(fmt_ctx);
      }
      return Err(msg);
    }

    let ret = unsafe { ffi::avcodec_parameters_from_context((*stream).codecpar, codec_ctx) };
    if ret < 0 {
      let msg = format!(
        "avcodec_parameters_from_context failed: {}",
        av_error_string(ret)
      );
      unsafe {
        ffi::avcodec_free_context(&mut codec_ctx);
        ffi::avformat_free_context(fmt_ctx);
      }
      return Err(msg);
    }
    unsafe { (*stream).time_base = encoder_time_base };

    let needs_file = unsafe { (*(*fmt_ctx).oformat).flags & ffi::AVFMT_NOFILE == 0 };
    if needs_file {
      let ret =
        unsafe { ffi::avio_open(&mut (*fmt_ctx).pb, dest_cstr.as_ptr(), ffi::AVIO_FLAG_WRITE) };
      if ret < 0 {
        let msg = format!("avio_open failed: {}", av_error_string(ret));
        unsafe {
          ffi::avcodec_free_context(&mut codec_ctx);
          ffi::avformat_free_context(fmt_ctx);
        }
        return Err(msg);
      }
    }

    let ret = unsafe { ffi::avformat_write_header(fmt_ctx, std::ptr::null_mut()) };
    if ret < 0 {
      let msg = format!("avformat_write_header failed: {}", av_error_string(ret));
      unsafe {
        if needs_file {
          ffi::avio_closep(&mut (*fmt_ctx).pb);
        }
        ffi::avcodec_free_context(&mut codec_ctx);
        ffi::avformat_free_context(fmt_ctx);
      }
      return Err(msg);
    }

    let sws_ctx = unsafe {
      ffi::sws_getContext(
        width as i32,
        height as i32,
        av_pixel_format(format),
        width as i32,
        height as i32,
        ffi::AVPixelFormat::AV_PIX_FMT_YUV420P,
        ffi::SWS_BILINEAR,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        std::ptr::null(),
      )
    };
    if sws_ctx.is_null() {
      unsafe {
        if needs_file {
          ffi::avio_closep(&mut (*fmt_ctx).pb);
        }
        ffi::avcodec_free_context(&mut codec_ctx);
        ffi::avformat_free_context(fmt_ctx);
      }
      return Err("sws_getContext failed".to_string());
    }

    let frame = unsafe { ffi::av_frame_alloc() };
    let packet = unsafe { ffi::av_packet_alloc() };
    unsafe {
      (*frame).format = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as i32;
      (*frame).width = width as i32;
      (*frame).height = height as i32;
    }
    let ret = unsafe { ffi::av_frame_get_buffer(frame, 32) };
    if ret < 0 {
      let msg = format!("av_frame_get_buffer failed: {}", av_error_string(ret));
      unsafe {
        ffi::av_packet_free(&mut (packet as *mut _));
        ffi::av_frame_free(&mut (frame as *mut _));
        ffi::sws_freeContext(sws_ctx);
        if needs_file {
          ffi::avio_closep(&mut (*fmt_ctx).pb);
        }
        ffi::avcodec_free_context(&mut codec_ctx);
        ffi::avformat_free_context(fmt_ctx);
      }
      return Err(msg);
    }

    Ok(RtmpsPublisher {
      fmt_ctx,
      codec_ctx,
      sws_ctx,
      frame,
      packet,
      stream_time_base: unsafe { (*stream).time_base },
      encoder_time_base,
      width,
      height,
      src_format: format,
      first_timestamp: None,
      opened_pb: needs_file,
    })
  }

  pub fn write_frame(&mut self, buf: &FrameBuf) -> Result<(), String> {
    if buf.format != self.src_format || buf.width != self.width || buf.height != self.height {
      return Err(format!(
        "frame is {}x{} ({:?}) but stream was opened for {}x{} ({:?}); \
         resolution/format can't change mid-stream",
        buf.width, buf.height, buf.format, self.width, self.height, self.src_format
      ));
    }

    let first_timestamp = *self.first_timestamp.get_or_insert(buf.timestamp);
    let elapsed = buf.timestamp.saturating_sub(first_timestamp);
    let pts = (elapsed.as_secs_f64() * self.encoder_time_base.den as f64
      / self.encoder_time_base.num as f64) as i64;

    let linesizes = plane_linesizes(buf.format, buf.width);
    let src_ptrs: Vec<*const u8> = buf.planes.iter().map(|p| p.as_ptr()).collect();

    unsafe {
      ffi::av_frame_make_writable(self.frame);
      ffi::sws_scale(
        self.sws_ctx,
        src_ptrs.as_ptr(),
        linesizes.as_ptr(),
        0,
        buf.height as i32,
        (*self.frame).data.as_ptr() as *const *mut u8,
        (*self.frame).linesize.as_ptr(),
      );
      (*self.frame).pts = pts;
    }

    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    draw_overlay_text(self.frame, &now);

    self.encode_and_write(self.frame)
  }

  fn flush(&mut self) -> Result<(), String> {
    self.encode_and_write(std::ptr::null_mut())
  }

  fn encode_and_write(&mut self, frame: *mut ffi::AVFrame) -> Result<(), String> {
    let ret = unsafe { ffi::avcodec_send_frame(self.codec_ctx, frame) };
    if ret < 0 {
      return Err(format!("avcodec_send_frame failed: {}", av_error_string(ret)));
    }

    loop {
      let ret = unsafe { ffi::avcodec_receive_packet(self.codec_ctx, self.packet) };
      if ret == AVERROR_EAGAIN || ret == AVERROR_EOF {
        break;
      }
      if ret < 0 {
        return Err(format!(
          "avcodec_receive_packet failed: {}",
          av_error_string(ret)
        ));
      }
      unsafe {
        ffi::av_packet_rescale_ts(self.packet, self.encoder_time_base, self.stream_time_base);
        (*self.packet).stream_index = 0;
        let ret = ffi::av_interleaved_write_frame(self.fmt_ctx, self.packet);
        ffi::av_packet_unref(self.packet);
        if ret < 0 {
          return Err(format!(
            "av_interleaved_write_frame failed: {}",
            av_error_string(ret)
          ));
        }
      }
    }
    Ok(())
  }
}

impl Drop for RtmpsPublisher {
  fn drop(&mut self) {
    let _ = self.flush();
    unsafe {
      ffi::av_write_trailer(self.fmt_ctx);
      ffi::sws_freeContext(self.sws_ctx);
      ffi::av_packet_free(&mut self.packet);
      ffi::av_frame_free(&mut self.frame);
      if self.opened_pb {
        ffi::avio_closep(&mut (*self.fmt_ctx).pb);
      }
      ffi::avcodec_free_context(&mut self.codec_ctx);
      ffi::avformat_free_context(self.fmt_ctx);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use skia_safe::EncodedImageFormat;

  // Renders the overlay onto a synthetic mid-gray frame and dumps the
  // result to a PNG for visual inspection, to check whether draw_overlay_text
  // is actually painting anything without needing the full RTMPS pipeline.
  #[test]
  fn overlay_smoke_test() {
    unsafe {
      let mut frame = ffi::av_frame_alloc();
      (*frame).format = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as i32;
      (*frame).width = 320;
      (*frame).height = 240;
      let ret = ffi::av_frame_get_buffer(frame, 32);
      assert!(ret >= 0, "av_frame_get_buffer failed: {}", ret);

      let linesize = (*frame).linesize[0] as usize;
      let height = (*frame).height as usize;
      let width = (*frame).width as usize;
      let y_data = (*frame).data[0];
      std::ptr::write_bytes(y_data, 128, linesize * height);

      draw_overlay_text(frame, "2026-07-20 12:34:56");

      let info = ImageInfo::new(
        ((*frame).width, (*frame).height),
        ColorType::Gray8,
        AlphaType::Opaque,
        None,
      );
      let y_plane = std::slice::from_raw_parts_mut(y_data, linesize * height);
      let mut surface = surfaces::wrap_pixels(&info, y_plane, linesize, None)
        .expect("failed to wrap Y plane for snapshot");
      let image = surface.image_snapshot();
      let data = image
        .encode(None, EncodedImageFormat::PNG, None)
        .expect("PNG encode failed");

      let path = std::env::temp_dir().join("rtmps_overlay_test.png");
      std::fs::write(&path, data.as_bytes()).unwrap();
      println!(
        "wrote {} ({}x{})",
        path.display(),
        width,
        height
      );

      ffi::av_frame_free(&mut frame);
    }
  }
}
