use std::ffi::{CStr, CString};
use std::marker::PhantomData;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use ffmpeg_sys_next as ffi;

use crate::api::{AnalogData, ImageData, ImageFormat, MocapData, NodeData};

#[derive(Clone, Debug)]
pub struct CameraInfo {
  pub device_name: String,
  pub description: String,
}

fn input_format_name() -> &'static str {
  if cfg!(target_os = "macos") {
    "avfoundation"
  } else if cfg!(target_os = "windows") {
    "dshow"
  } else {
    "v4l2"
  }
}

// avdevice_list_input_sources gives back the bare device name/path, but
// dshow's read_header (parse_device_name in dshow.c) requires the
// avformat_open_input filename to be "video=<name>" -- passing the bare name
// trips its strtok(url, "=") parsing and fails with "Malformed dshow input
// string." v4l2 (a plain path like /dev/video0) and avfoundation (which
// just av_strdup()s the url as-is in its own parse_device_name) both take
// the bare name directly.
fn device_url(device_name: &str) -> String {
  if cfg!(target_os = "windows") {
    format!("video={}", device_name)
  } else {
    device_name.to_string()
  }
}

// Some devices (e.g. a webcam's separate metadata/UVC-extension node) show up
// in avdevice_list_input_sources with the same description as the camera's
// real capture node, but don't actually produce a video stream. Filter those
// out by briefly opening the device and checking for one.
//
// Deliberately skips avformat_find_stream_info: capture device formats (unlike
// container formats) already know their stream's codec_type/width/height from
// device ioctls by the time avformat_open_input returns, and find_stream_info
// would needlessly start the device's capture pipeline (queue buffers, stream
// on) just to immediately tear it down again.
fn has_video_stream(input_fmt: *const ffi::AVInputFormat, device_name: &str) -> bool {
  let Ok(device_cstr) = CString::new(device_url(device_name)) else {
    return false;
  };
  let mut fmt_ctx: *mut ffi::AVFormatContext = std::ptr::null_mut();

  let ret = unsafe {
    ffi::avformat_open_input(
      &mut fmt_ctx,
      device_cstr.as_ptr(),
      input_fmt,
      std::ptr::null_mut(),
    )
  };
  if ret < 0 || fmt_ctx.is_null() {
    return false;
  }

  let has_video = unsafe {
    let streams = std::slice::from_raw_parts((*fmt_ctx).streams, (*fmt_ctx).nb_streams as usize);
    streams
      .iter()
      .any(|&stream| (*(*stream).codecpar).codec_type == ffi::AVMediaType::AVMEDIA_TYPE_VIDEO)
  };

  unsafe { ffi::avformat_close_input(&mut fmt_ctx) };
  has_video
}

pub fn list_cameras() -> Vec<CameraInfo> {
  unsafe { ffi::avdevice_register_all() };

  let format_name = CString::new(input_format_name()).unwrap();
  let input_fmt = unsafe { ffi::av_find_input_format(format_name.as_ptr()) };
  if input_fmt.is_null() {
    println!(
      "WebcamNode: input format '{}' not found",
      input_format_name()
    );
    return Vec::new();
  }

  let mut device_list: *mut ffi::AVDeviceInfoList = std::ptr::null_mut();
  let ret = unsafe {
    ffi::avdevice_list_input_sources(
      input_fmt,
      std::ptr::null(),
      std::ptr::null_mut(),
      &mut device_list,
    )
  };
  if ret < 0 || device_list.is_null() {
    println!("WebcamNode: avdevice_list_input_sources failed ({})", ret);
    return Vec::new();
  }

  let count = unsafe { (*device_list).nb_devices as usize };
  // devices can be null when count == 0 (e.g. no cameras present), and
  // from_raw_parts requires a non-null pointer even for a zero-length slice.
  let cameras = if count == 0 {
    Vec::new()
  } else {
    let devices = unsafe { std::slice::from_raw_parts((*device_list).devices, count) };
    devices
      .iter()
      .filter_map(|&device_ptr| {
        let device = unsafe { &*device_ptr };
        let device_name = unsafe {
          CStr::from_ptr(device.device_name)
            .to_string_lossy()
            .to_string()
        };
        if !has_video_stream(input_fmt, &device_name) {
          return None;
        }
        let description = unsafe {
          CStr::from_ptr(device.device_description)
            .to_string_lossy()
            .to_string()
        };
        Some(CameraInfo {
          device_name,
          description,
        })
      })
      .collect()
  };

  unsafe { ffi::avdevice_free_list_devices(&mut device_list) };

  cameras
}

// va_list decays to a pointer when used as a C function parameter (System V
// x86-64 ABI), but ffi::va_list is the raw, non-decayed array typedef, and
// Rust doesn't apply C's array-to-pointer decay -- so these declarations use
// the decayed pointer type directly to match the real ABI these functions
// expect.
unsafe extern "C" {
  fn vsnprintf(buf: *mut c_char, size: usize, fmt: *const c_char, args: *mut ffi::__va_list_tag) -> c_int;
}

// Accumulates messages logged (at or above PROBE_LOG_LEVEL) during a
// probe_resolution()/list_formats() call, e.g. dshow's "Could not set video
// options" or its list_options format dump -- far more actionable than the
// generic errno string avformat_open_input's return code decodes to.
static PROBE_LOG: Mutex<String> = Mutex::new(String::new());
static PROBE_LOG_LEVEL: AtomicI32 = AtomicI32::new(ffi::AV_LOG_ERROR);
// av_log_set_callback is process-global, so serialize probes against each
// other to keep one probe's messages from bleeding into another's.
static PROBE_GUARD: Mutex<()> = Mutex::new(());

unsafe extern "C" fn probe_log_callback(
  _avcl: *mut c_void,
  level: c_int,
  fmt: *const c_char,
  args: *mut ffi::__va_list_tag,
) {
  if level > PROBE_LOG_LEVEL.load(Ordering::Relaxed) {
    return;
  }
  let mut buf = [0 as c_char; 512];
  let n = unsafe { vsnprintf(buf.as_mut_ptr(), buf.len(), fmt, args) };
  if n <= 0 {
    return;
  }
  let len = (n as usize).min(buf.len() - 1);
  let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, len) };
  if let Ok(mut log) = PROBE_LOG.lock() {
    log.push_str(&String::from_utf8_lossy(bytes));
  }
}

fn av_error_string(ret: i32) -> String {
  let mut buf = [0 as c_char; ffi::AV_ERROR_MAX_STRING_SIZE];
  let rc = unsafe { ffi::av_strerror(ret, buf.as_mut_ptr(), buf.len()) };
  if rc == 0 {
    unsafe { CStr::from_ptr(buf.as_ptr()) }
      .to_string_lossy()
      .to_string()
  } else {
    format!("error code {}", ret)
  }
}

// Briefly opens the device requesting the given resolution, then closes it
// again. There's no capability-query API in this FFmpeg version (removed
// upstream between 5.1 and 6.1), so "does the device support this
// resolution" can only be answered by trying it and seeing whether the
// driver accepts it.
pub fn probe_resolution(device_name: &str, width: i32, height: i32) -> Result<(), String> {
  let _guard = PROBE_GUARD.lock().unwrap_or_else(|e| e.into_inner());

  unsafe { ffi::avdevice_register_all() };

  let format_name = CString::new(input_format_name()).unwrap();
  let input_fmt = unsafe { ffi::av_find_input_format(format_name.as_ptr()) };
  if input_fmt.is_null() {
    return Err(format!("input format '{}' not found", input_format_name()));
  }

  let device_cstr = CString::new(device_url(device_name)).map_err(|e| e.to_string())?;

  let mut options: *mut ffi::AVDictionary = std::ptr::null_mut();
  let video_size_key = CString::new("video_size").unwrap();
  let video_size_val = CString::new(format!("{}x{}", width, height)).unwrap();
  unsafe {
    ffi::av_dict_set(
      &mut options,
      video_size_key.as_ptr(),
      video_size_val.as_ptr(),
      0,
    );
  }

  *PROBE_LOG.lock().unwrap() = String::new();
  PROBE_LOG_LEVEL.store(ffi::AV_LOG_ERROR, Ordering::Relaxed);
  unsafe { ffi::av_log_set_callback(Some(probe_log_callback)) };

  let mut fmt_ctx: *mut ffi::AVFormatContext = std::ptr::null_mut();
  let ret = unsafe {
    ffi::avformat_open_input(&mut fmt_ctx, device_cstr.as_ptr(), input_fmt, &mut options)
  };
  unsafe { ffi::av_dict_free(&mut options) };
  unsafe { ffi::av_log_set_callback(Some(ffi::av_log_default_callback)) };

  if ret >= 0 && !fmt_ctx.is_null() {
    unsafe { ffi::avformat_close_input(&mut fmt_ctx) };
    return Ok(());
  }

  let captured = PROBE_LOG.lock().unwrap().trim().to_string();
  if !captured.is_empty() {
    Err(captured)
  } else {
    Err(av_error_string(ret))
  }
}

// Opens the device with its "dump every supported mode" debug option set
// (dshow: list_options, v4l2: list_formats=all) so the driver logs each
// pixel format / resolution / frame-rate combination it supports, and
// returns that dump as text. This is the same information `ffmpeg -f dshow
// -list_options true -i ...` prints on the CLI -- there's no structured
// capability-query API to pull it from instead (see probe_resolution).
// avfoundation has no equivalent listing option.
pub fn list_formats(device_name: &str) -> Result<String, String> {
  let _guard = PROBE_GUARD.lock().unwrap_or_else(|e| e.into_inner());

  unsafe { ffi::avdevice_register_all() };

  let format_name = CString::new(input_format_name()).unwrap();
  let input_fmt = unsafe { ffi::av_find_input_format(format_name.as_ptr()) };
  if input_fmt.is_null() {
    return Err(format!("input format '{}' not found", input_format_name()));
  }

  let (opt_key, opt_val) = match input_format_name() {
    "dshow" => ("list_options", "true"),
    "v4l2" => ("list_formats", "all"),
    other => return Err(format!("listing formats is not supported for '{}'", other)),
  };

  let device_cstr = CString::new(device_url(device_name)).map_err(|e| e.to_string())?;

  let mut options: *mut ffi::AVDictionary = std::ptr::null_mut();
  let opt_key_cstr = CString::new(opt_key).unwrap();
  let opt_val_cstr = CString::new(opt_val).unwrap();
  unsafe {
    ffi::av_dict_set(&mut options, opt_key_cstr.as_ptr(), opt_val_cstr.as_ptr(), 0);
  }

  *PROBE_LOG.lock().unwrap() = String::new();
  PROBE_LOG_LEVEL.store(ffi::AV_LOG_INFO, Ordering::Relaxed);
  unsafe { ffi::av_log_set_callback(Some(probe_log_callback)) };

  let mut fmt_ctx: *mut ffi::AVFormatContext = std::ptr::null_mut();
  let ret = unsafe {
    ffi::avformat_open_input(&mut fmt_ctx, device_cstr.as_ptr(), input_fmt, &mut options)
  };
  unsafe { ffi::av_dict_free(&mut options) };
  unsafe { ffi::av_log_set_callback(Some(ffi::av_log_default_callback)) };
  PROBE_LOG_LEVEL.store(ffi::AV_LOG_ERROR, Ordering::Relaxed);

  if !fmt_ctx.is_null() {
    unsafe { ffi::avformat_close_input(&mut fmt_ctx) };
  }

  // list_options/list_formats deliberately abort the open after printing
  // (dshow returns AVERROR_EXIT), so a negative ret here is expected, not a
  // real failure -- only treat it as an error if nothing was captured.
  let captured = PROBE_LOG.lock().unwrap().trim().to_string();
  if !captured.is_empty() {
    Ok(captured)
  } else {
    Err(av_error_string(ret))
  }
}

// Per-plane (row_bytes, rows), tightly packed (no linesize padding).
fn plane_layout(format: ImageFormat, width: u64, height: u64) -> Vec<(u64, u64)> {
  match format {
    ImageFormat::Gray => vec![(width, height)],
    ImageFormat::RGB => vec![(width * 3, height)],
    ImageFormat::YUYV422 => vec![(width * 2, height)],
    ImageFormat::YUV420P | ImageFormat::YUVJ420P => {
      let chroma_w = width.div_ceil(2);
      let chroma_h = height.div_ceil(2);
      vec![(width, height), (chroma_w, chroma_h), (chroma_w, chroma_h)]
    }
  }
}

fn map_pixel_format(fmt: i32) -> Option<ImageFormat> {
  if fmt == ffi::AVPixelFormat::AV_PIX_FMT_GRAY8 as i32 {
    Some(ImageFormat::Gray)
  } else if fmt == ffi::AVPixelFormat::AV_PIX_FMT_RGB24 as i32 {
    Some(ImageFormat::RGB)
  } else if fmt == ffi::AVPixelFormat::AV_PIX_FMT_YUYV422 as i32 {
    Some(ImageFormat::YUYV422)
  } else if fmt == ffi::AVPixelFormat::AV_PIX_FMT_YUV420P as i32 {
    Some(ImageFormat::YUV420P)
  } else if fmt == ffi::AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32 {
    Some(ImageFormat::YUVJ420P)
  } else {
    None
  }
}

// Borrows straight from the AVFrame buffer owned by the WebcamCapture that
// produced it -- no per-row copy. The 'a lifetime ties this to that
// WebcamCapture's &mut borrow, so the borrow checker prevents calling
// read_frame() again (which would unref/refill the same AVFrame) while a
// CapturedFrame from a previous call is still alive. Dropping releases the
// frame's buffer ref, so callers should consume it promptly (e.g. within a
// single synchronous ready_offmain call) rather than holding onto it.
pub struct CapturedFrame<'a> {
  frame: *mut ffi::AVFrame,
  planes: Vec<&'a [u8]>,
  format: ImageFormat,
  width: u64,
  height: u64,
  time: Duration,
  frame_interval: Duration,
  _capture: PhantomData<&'a mut WebcamCapture>,
}

impl<'a> CapturedFrame<'a> {
  pub fn with_time(mut self, time: Duration) -> Self {
    self.time = time;
    self
  }
}

impl<'a> Drop for CapturedFrame<'a> {
  fn drop(&mut self) {
    unsafe { ffi::av_frame_unref(self.frame) };
  }
}

impl<'a> NodeData for CapturedFrame<'a> {
  fn time(&self) -> Duration {
    self.time
  }
  fn analog(&self) -> Option<&dyn AnalogData> {
    None
  }
  fn image(&self) -> Option<&dyn ImageData> {
    Some(self)
  }
  fn mocap(&self) -> Option<&dyn MocapData> {
    None
  }
}

impl<'a> ImageData for CapturedFrame<'a> {
  fn plane(&self, channel: i32) -> &[u8] {
    self.planes.get(channel as usize).copied().unwrap_or(&[])
  }
  fn num_planes(&self) -> u64 {
    self.planes.len() as u64
  }
  fn format(&self) -> ImageFormat {
    self.format
  }
  fn width(&self) -> u64 {
    self.width
  }
  fn height(&self) -> u64 {
    self.height
  }
  fn frame_interval(&self) -> Duration {
    self.frame_interval
  }
}

pub struct WebcamCapture {
  fmt_ctx: *mut ffi::AVFormatContext,
  codec_ctx: *mut ffi::AVCodecContext,
  packet: *mut ffi::AVPacket,
  frame: *mut ffi::AVFrame,
  video_stream_index: i32,
  frame_interval: Duration,
}

impl WebcamCapture {
  pub fn open(device_name: &str, width: u32, height: u32) -> Result<Self, String> {
    unsafe { ffi::avdevice_register_all() };

    let format_name = CString::new(input_format_name()).unwrap();
    let input_fmt = unsafe { ffi::av_find_input_format(format_name.as_ptr()) };
    if input_fmt.is_null() {
      return Err(format!("input format '{}' not found", input_format_name()));
    }

    let device_cstr = CString::new(device_url(device_name)).map_err(|e| e.to_string())?;

    // Without an explicit request, v4l2 just inherits whatever mode the
    // device is currently sitting in (e.g. a prior session/tool may have
    // left it at a high-res/uncompressed/low-fps combination), which can
    // silently cap the achievable frame rate. Request the caller's
    // resolution and a 30fps rate instead of trusting the device's current
    // state.
    let mut options: *mut ffi::AVDictionary = std::ptr::null_mut();
    let video_size_key = CString::new("video_size").unwrap();
    let video_size_val = CString::new(format!("{}x{}", width, height)).unwrap();
    let framerate_key = CString::new("framerate").unwrap();
    let framerate_val = CString::new("30").unwrap();
    unsafe {
      ffi::av_dict_set(
        &mut options,
        video_size_key.as_ptr(),
        video_size_val.as_ptr(),
        0,
      );
      ffi::av_dict_set(
        &mut options,
        framerate_key.as_ptr(),
        framerate_val.as_ptr(),
        0,
      );
    }

    let mut fmt_ctx: *mut ffi::AVFormatContext = std::ptr::null_mut();
    let ret = unsafe {
      ffi::avformat_open_input(&mut fmt_ctx, device_cstr.as_ptr(), input_fmt, &mut options)
    };
    unsafe { ffi::av_dict_free(&mut options) };
    if ret < 0 || fmt_ctx.is_null() {
      return Err(format!("avformat_open_input failed ({})", ret));
    }

    if unsafe { ffi::avformat_find_stream_info(fmt_ctx, std::ptr::null_mut()) } < 0 {
      unsafe { ffi::avformat_close_input(&mut fmt_ctx) };
      return Err("avformat_find_stream_info failed".to_string());
    }

    let mut decoder: *const ffi::AVCodec = std::ptr::null();
    let video_stream_index = unsafe {
      ffi::av_find_best_stream(
        fmt_ctx,
        ffi::AVMediaType::AVMEDIA_TYPE_VIDEO,
        -1,
        -1,
        &mut decoder,
        0,
      )
    };
    if video_stream_index < 0 || decoder.is_null() {
      unsafe { ffi::avformat_close_input(&mut fmt_ctx) };
      return Err("no video stream found".to_string());
    }

    let mut codec_ctx = unsafe { ffi::avcodec_alloc_context3(decoder) };
    if codec_ctx.is_null() {
      unsafe { ffi::avformat_close_input(&mut fmt_ctx) };
      return Err("avcodec_alloc_context3 failed".to_string());
    }

    let stream = unsafe { *(*fmt_ctx).streams.offset(video_stream_index as isize) };
    if unsafe { ffi::avcodec_parameters_to_context(codec_ctx, (*stream).codecpar) } < 0 {
      unsafe {
        ffi::avcodec_free_context(&mut codec_ctx);
        ffi::avformat_close_input(&mut fmt_ctx);
      }
      return Err("avcodec_parameters_to_context failed".to_string());
    }

    if unsafe { ffi::avcodec_open2(codec_ctx, decoder, std::ptr::null_mut()) } < 0 {
      unsafe {
        ffi::avcodec_free_context(&mut codec_ctx);
        ffi::avformat_close_input(&mut fmt_ctx);
      }
      return Err("avcodec_open2 failed".to_string());
    }

    let avg_frame_rate = unsafe { (*stream).avg_frame_rate };
    let frame_interval = if avg_frame_rate.num != 0 && avg_frame_rate.den != 0 {
      Duration::from_secs_f64(avg_frame_rate.den as f64 / avg_frame_rate.num as f64)
    } else {
      Duration::from_secs_f64(1.0 / 30.0)
    };

    let packet = unsafe { ffi::av_packet_alloc() };
    let frame = unsafe { ffi::av_frame_alloc() };

    Ok(WebcamCapture {
      fmt_ctx,
      codec_ctx,
      packet,
      frame,
      video_stream_index,
      frame_interval,
    })
  }

  // Reads/decodes until a frame in a supported pixel format is produced, or
  // the stream ends/errors. Returns None to signal the caller should stop.
  pub fn read_frame(&mut self) -> Option<CapturedFrame<'_>> {
    loop {
      if unsafe { ffi::av_read_frame(self.fmt_ctx, self.packet) } < 0 {
        return None;
      }

      if unsafe { (*self.packet).stream_index } != self.video_stream_index {
        unsafe { ffi::av_packet_unref(self.packet) };
        continue;
      }

      let send_ret = unsafe { ffi::avcodec_send_packet(self.codec_ctx, self.packet) };
      unsafe { ffi::av_packet_unref(self.packet) };
      if send_ret < 0 {
        continue;
      }

      if unsafe { ffi::avcodec_receive_frame(self.codec_ctx, self.frame) } < 0 {
        continue;
      }

      let format = match map_pixel_format(unsafe { (*self.frame).format }) {
        Some(format) => format,
        None => {
          println!(
            "WebcamNode: unsupported pixel format {}, dropping frame",
            unsafe { (*self.frame).format }
          );
          unsafe { ffi::av_frame_unref(self.frame) };
          continue;
        }
      };

      let width = unsafe { (*self.frame).width } as u64;
      let height = unsafe { (*self.frame).height } as u64;

      let planes = plane_layout(format, width, height)
        .into_iter()
        .enumerate()
        .map(|(channel, (_, rows))| {
          let data_ptr = unsafe { (*self.frame).data[channel] };
          let linesize = unsafe { (*self.frame).linesize[channel] } as u64;
          if data_ptr.is_null() {
            return &[][..];
          }
          let len = (linesize * rows) as usize;
          unsafe { std::slice::from_raw_parts(data_ptr, len) }
        })
        .collect();

      return Some(CapturedFrame {
        frame: self.frame,
        planes,
        format,
        width,
        height,
        time: Duration::ZERO,
        frame_interval: self.frame_interval,
        _capture: PhantomData,
      });
    }
  }
}

impl Drop for WebcamCapture {
  fn drop(&mut self) {
    unsafe {
      if !self.frame.is_null() {
        ffi::av_frame_free(&mut self.frame);
      }
      if !self.packet.is_null() {
        ffi::av_packet_free(&mut self.packet);
      }
      if !self.codec_ctx.is_null() {
        ffi::avcodec_free_context(&mut self.codec_ctx);
      }
      if !self.fmt_ctx.is_null() {
        ffi::avformat_close_input(&mut self.fmt_ctx);
      }
    }
  }
}
