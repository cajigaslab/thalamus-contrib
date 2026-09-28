use nokhwa::pixel_format::RgbFormat;
use std::{
  cell::RefCell,
  rc::{Rc, Weak},
  sync::{Arc, Mutex},
  time::Duration,
};

use crate::api::{
  self, ImageData, Json, MainThreadOnly, MainThreadToken, Node, NodeConsts, NodeData, NodeToken,
  OffMainSignaler, OnDrop, Request, State, StateAction, StateValue, THALAMUS_MODALITY_IMAGE,
  ThalamusAPI, ThalamusAPIThreadSafe,
};
use crate::image_viewer::{ImageSink, ImageViewer};

struct Frame<'a> {
  // Borrowed straight from nokhwa's buffer; only the first num_planes are used.
  planes: [&'a [u8]; 2],
  num_planes: u64,
  width: u64,
  height: u64,
  format: api::ImageFormat,
  time: Duration,
  frame_interval: Duration,
}

impl<'a> NodeData for Frame<'a> {
  fn analog(&self) -> Option<&dyn api::AnalogData> {
    None
  }

  fn image(&self) -> Option<&dyn ImageData> {
    Some(self)
  }

  fn mocap(&self) -> Option<&dyn api::MocapData> {
    None
  }

  fn text(&self) -> Option<&dyn api::TextData> {
    None
  }

  fn time(&self) -> Duration {
    self.time
  }
}

impl<'a> ImageData for Frame<'a> {
  fn plane(&self, channel: i32) -> &[u8] {
    match usize::try_from(channel) {
      Ok(i) if (i as u64) < self.num_planes => self.planes[i],
      _ => &[],
    }
  }

  fn num_planes(&self) -> u64 {
    self.num_planes
  }

  fn format(&self) -> api::ImageFormat {
    self.format
  }

  fn width(&self) -> u64 {
    self.width
  }

  fn height(&self) -> u64 {
    self.height
  }

  fn frame_interval(&self) -> std::time::Duration {
    self.frame_interval
  }
}

fn frame_format_name(format: nokhwa::utils::FrameFormat) -> &'static str {
  match format {
    nokhwa::utils::FrameFormat::MJPEG => "MJPEG",
    nokhwa::utils::FrameFormat::YUYV => "YUYV",
    nokhwa::utils::FrameFormat::NV12 => "NV12",
    nokhwa::utils::FrameFormat::GRAY => "GRAY",
    nokhwa::utils::FrameFormat::RAWRGB => "RAWRGB",
    nokhwa::utils::FrameFormat::RAWBGR => "RAWBGR",
  }
}

/// The object stored in the node's "Format" config and returned by get_formats.
fn camera_format_json(format: &nokhwa::utils::CameraFormat) -> serde_json::Value {
  serde_json::json!({
    "width": format.width(),
    "height": format.height(),
    "frame_rate": format.frame_rate(),
    "format": frame_format_name(format.format()),
  })
}

/// Formats reported by a camera, largest resolution and highest frame rate first.
fn query_formats(
  camera: &mut nokhwa::Camera,
) -> Result<Vec<nokhwa::utils::CameraFormat>, nokhwa::NokhwaError> {
  let mut formats = camera.compatible_camera_formats()?;
  formats.sort_by_key(|f| {
    (
      std::cmp::Reverse(f.width() as u64 * f.height() as u64),
      std::cmp::Reverse(f.width()),
      std::cmp::Reverse(f.frame_rate()),
      f.format(),
    )
  });
  formats.dedup();
  Ok(formats)
}

fn get_cameras() -> Vec<serde_json::Value> {
  let cameras = match nokhwa::query(nokhwa::utils::ApiBackend::Auto) {
    Ok(cameras) => cameras,
    Err(e) => {
      println!("Camera query failed: {:?}", e);
      Vec::new()
    }
  };
  cameras
    .iter()
    .filter_map(|camera| {
      let index = camera.index().as_index().ok()?;
      Some(serde_json::json!({
        "index": index,
        "name": camera.human_name(),
        "description": camera.description(),
      }))
    })
    .collect()
}

/// The formats of the camera the webcam thread currently has open.  Opening a
/// camera a second time while it is streaming can fail, so get_formats answers
/// from here for that camera.
type ActiveFormats = Arc<Mutex<Option<(u32, Vec<nokhwa::utils::CameraFormat>)>>>;

/// Snapshot of the node's config, read on the main thread and handed to the webcam thread.
struct WebcamSettings {
  index: Option<u32>,
  width: u32,
  height: u32,
  frame_rate: f64,
  format: Option<String>,
}

impl WebcamSettings {
  fn read(state: &State) -> WebcamSettings {
    // The widget stores one of the objects returned by get_formats.
    let format = match state.get(api::StateKey::String("Format".to_string())) {
      Some(StateValue::Dict(format)) => Some(format),
      _ => None,
    };
    let number = |key: &str, default: f64| -> f64 {
      format
        .as_ref()
        .and_then(|f| f.get(api::StateKey::String(key.to_string())))
        .and_then(|v| f64::try_from(v).ok())
        .unwrap_or(default)
    };
    let frame_format = match format
      .as_ref()
      .and_then(|f| f.get(api::StateKey::String("format".to_string())))
    {
      Some(StateValue::String(s)) => Some(s),
      _ => None,
    };

    // The widget stores the object returned by get_cameras, index is -1 when
    // nothing has been selected.
    let index = match state.get(api::StateKey::String("Camera".to_string())) {
      Some(StateValue::Dict(camera)) => {
        match camera.get(api::StateKey::String("index".to_string())) {
          Some(StateValue::Int(i)) => u32::try_from(i).ok(),
          _ => None,
        }
      }
      _ => None,
    };

    WebcamSettings {
      index,
      width: number("width", 640.0) as u32,
      height: number("height", 480.0) as u32,
      frame_rate: number("frame_rate", 30.0),
      format: frame_format,
    }
  }
}

pub struct WebcamNode {
  api: ThalamusAPI,
  _state_connection: OnDrop,
  main_thread_token: MainThreadToken,
  state: State,
  signaler: Arc<OffMainSignaler>,
  webcam_thread: Option<std::thread::JoinHandle<()>>,
  active_formats: ActiveFormats,
  // Open while View is true; the capture thread sends it every frame.
  viewer: Option<ImageViewer>,
  viewer_sink: ImageSink,
}

impl WebcamNode {
  fn stop_webcam(&mut self) {
    self.signaler.block();
    self.webcam_thread.take().map(|h| h.join());
  }

  fn webcam(
    api: ThalamusAPIThreadSafe,
    signaler: Arc<OffMainSignaler>,
    settings: WebcamSettings,
    active_formats: ActiveFormats,
    viewer_sink: ImageSink,
  ) {
    println!("webcam start");
    let Some(index) = settings.index else {
      println!("No camera selected");
      return;
    };
    // The format is chosen below, this request only has to get the camera open.
    let format =
      nokhwa::utils::RequestedFormat::new::<RgbFormat>(nokhwa::utils::RequestedFormatType::None);
    let mut camera = match nokhwa::Camera::new(nokhwa::utils::CameraIndex::Index(index), format) {
      Ok(c) => c,
      Err(e) => {
        println!("Camera Selection failed: {:?}", e);
        return;
      }
    };

    // The configured format normally comes straight from get_formats, but it may be from another
    // camera, so pick the closest format the camera actually has.  Resolution
    // takes priority over frame rate, which takes priority over the pixel format.
    //
    // The frame interval comes from the format we request rather than from
    // camera.frame_rate(): nokhwa's Media Foundation backend
    // (nokhwa-bindings-windows 0.4.6, format_refreshed) keeps only the low 32
    // bits of MF_MT_FRAME_RATE, i.e. the ratio's denominator, so it reports
    // 1 fps for every camera on Windows.
    let frame_rate;
    match query_formats(&mut camera) {
      Ok(formats) => {
        *active_formats.lock().unwrap() = Some((index, formats.clone()));
        let best = formats.into_iter().min_by_key(|f| {
          let width_diff = f.width() as i64 - settings.width as i64;
          let height_diff = f.height() as i64 - settings.height as i64;
          let resolution_distance = width_diff * width_diff + height_diff * height_diff;
          let frame_rate_distance =
            ((f.frame_rate() as f64 - settings.frame_rate).abs() * 1000.0) as u64;
          let format_mismatch = settings
            .format
            .as_deref()
            .is_some_and(|name| name != frame_format_name(f.format()));
          (resolution_distance, frame_rate_distance, format_mismatch)
        });
        match best {
          Some(best) => {
            println!(
              "Requested {}x{} @ {} Hz, using {}",
              settings.width, settings.height, settings.frame_rate, best
            );
            let allowed = [best.format()];
            let exact = nokhwa::utils::RequestedFormat::with_formats(
              nokhwa::utils::RequestedFormatType::Exact(best),
              &allowed,
            );
            match camera.set_camera_requset(exact) {
              Ok(format) => frame_rate = format.frame_rate(),
              Err(e) => {
                println!("Camera Format Selection failed: {:?}", e);
                return;
              }
            }
          }
          None => {
            println!("Camera reported no formats");
            return;
          }
        }
      }
      Err(e) => {
        println!("Camera Format Query failed: {:?}", e);
        return;
      }
    }

    // Zero means unknown to consumers (e.g. storage falls back to a default).
    let frame_interval = if frame_rate > 0 {
      Duration::from_secs_f64(1.0 / frame_rate as f64)
    } else {
      Duration::ZERO
    };

    match camera.open_stream() {
      Ok(_) => {}
      Err(e) => {
        println!("Camera Open failed: {:?}", e);
        return;
      }
    };

    loop {
      let buffer = match camera.frame() {
        Ok(v) => v,
        Err(e) => {
          println!("Frame Grab failed: {:?}", e);
          return;
        }
      };
      let time = api.time();

      let format = match buffer.source_frame_format() {
        nokhwa::utils::FrameFormat::MJPEG => api::ImageFormat::MJPEG,
        nokhwa::utils::FrameFormat::YUYV => api::ImageFormat::YUYV422,
        nokhwa::utils::FrameFormat::NV12 => api::ImageFormat::NV12,
        nokhwa::utils::FrameFormat::GRAY => api::ImageFormat::Gray,
        nokhwa::utils::FrameFormat::RAWRGB => api::ImageFormat::RGB,
        nokhwa::utils::FrameFormat::RAWBGR => api::ImageFormat::BGR,
      };

      let resolution = buffer.resolution();
      //println!("{} {} {} {}", buffer.buffer().len(), resolution.width(), resolution.height(), buffer.source_frame_format());
      let data = buffer.buffer();
      let (planes, num_planes) = if format == api::ImageFormat::NV12 {
        // nokhwa delivers NV12 as one tightly packed buffer, the Y plane
        // (width x height) followed by the interleaved UV plane
        // (width x height/2); forward them as separate planes.
        let luma_size = resolution.width() as usize * resolution.height() as usize;
        if data.len() < luma_size * 3 / 2 {
          println!(
            "NV12 frame has {} bytes, expected {}, dropping it",
            data.len(),
            luma_size * 3 / 2
          );
          continue;
        }
        let (y, uv) = data.split_at(luma_size);
        ([y, &uv[..luma_size / 2]], 2)
      } else {
        ([data, &[][..]], 1)
      };
      let frame = Frame {
        planes,
        num_planes,
        width: resolution.width() as u64,
        height: resolution.height() as u64,
        time,
        format,
        frame_interval,
      };
      // MJPEG frames are dropped by the viewer.
      viewer_sink.update(&frame);
      match signaler.ready(&frame) {
        Ok(v) => {
          if !v {
            println!("Blocked");
            break;
          }
        }
        Err(_) => break,
      }
    }
    println!("webcam end");
  }

  /// Borrows the node only in the arms that need it: opening the viewer
  /// writes view_geometry to the state, which calls back into this.
  fn on_state(rc: &Rc<RefCell<Self>>, _source: State, _action: StateAction, key: StateValue, value: StateValue) {
    let StateValue::String(key_str) = key else {
      return;
    };
    match key_str.as_str() {
      "View" => {
        let mut this = rc.borrow_mut();
        if value != StateValue::Bool(true) {
          this.viewer = None;
        } else if this.viewer.is_none() {
          let (api, state, sink, token) =
            (this.api, this.state.clone(), this.viewer_sink.clone(), this.main_thread_token);
          drop(this);
          match ImageViewer::new(api, state, &sink, token) {
            Ok(viewer) => rc.borrow_mut().viewer = Some(viewer),
            Err(e) => println!("WebcamNode: failed to create image viewer: {e}"),
          }
        }
      }
      "Running" => {
        let mut this = rc.borrow_mut();
        let this = &mut *this;
        this.stop_webcam();
        if value == StateValue::Bool(true) {
          let api = this.api.thread_safe();
          let signaler = this.signaler.clone();
          signaler.unblock();
          let wrapped_state = MainThreadOnly::new(this.state.clone(), this.main_thread_token);
          let settings = WebcamSettings::read(&this.state);
          let active_formats = this.active_formats.clone();
          let viewer_sink = this.viewer_sink.clone();
          this.webcam_thread = Some(std::thread::spawn(move || {
            WebcamNode::webcam(api, signaler, settings, active_formats.clone(), viewer_sink);
            *active_formats.lock().unwrap() = None;
            api.post_to_main(|main_thread_token| {
              let state = wrapped_state.take(main_thread_token);
              state.set(
                api::StateKey::String("Running".to_string()),
                api::StateValue::Bool(false),
              );
            });
          }));
        }
      }
      _ => {}
    }
  }
}

impl WebcamNode {
  fn get_formats(&self, index: u32) -> Vec<serde_json::Value> {
    if let Some((active_index, formats)) = self.active_formats.lock().unwrap().as_ref() {
      if *active_index == index {
        return formats.iter().map(camera_format_json).collect();
      }
    }

    let request =
      nokhwa::utils::RequestedFormat::new::<RgbFormat>(nokhwa::utils::RequestedFormatType::None);
    let mut camera = match nokhwa::Camera::new(nokhwa::utils::CameraIndex::Index(index), request) {
      Ok(camera) => camera,
      Err(e) => {
        println!("Camera {} open for format query failed: {:?}", index, e);
        return Vec::new();
      }
    };
    match query_formats(&mut camera) {
      Ok(formats) => formats.iter().map(camera_format_json).collect(),
      Err(e) => {
        println!("Camera {} format query failed: {:?}", index, e);
        Vec::new()
      }
    }
  }
}

impl NodeConsts for WebcamNode {
  const MODALITIES: u32 = THALAMUS_MODALITY_IMAGE;
  const SIGNALS_OFFMAIN: bool = true;
}

impl WebcamNode {
  fn process(&mut self, handle: Request, request: Json) {
    let api = self.api;
    let request = serde_json::from_str::<serde_json::Value>(&request.to_string())
      .unwrap_or(serde_json::Value::Null);
    // Requests are either a bare string naming the request, or an object whose "type" names it.
    let request_type = match &request {
      serde_json::Value::String(s) => Some(s.as_str()),
      serde_json::Value::Object(o) => o.get("type").and_then(|t| t.as_str()),
      _ => None,
    };
    let response = match request_type {
      Some("get_cameras") => serde_json::to_string(&get_cameras()).unwrap(),
      Some("get_formats") => {
        match request
          .get("index")
          .and_then(|i| i.as_u64())
          .and_then(|i| u32::try_from(i).ok())
        {
          Some(index) => serde_json::to_string(&self.get_formats(index)).unwrap(),
          None => {
            println!(
              "get_formats requires a non-negative integer index: {}",
              request
            );
            "[]".to_string()
          }
        }
      }
      _ => "null".to_string(),
    };
    handle.respond(&Json::from_string(api, &response));
  }
}

impl Node for WebcamNode {
  fn new(
    api: ThalamusAPI,
    node_token: NodeToken,
    state: State,
    main_thread_token: MainThreadToken,
  ) -> Rc<RefCell<Self>> {
    let result = Rc::new_cyclic(|weak: &Weak<RefCell<Self>>| {
      let signaler = OffMainSignaler::new(api, node_token.clone());

      let weak2 = weak.clone();
      let callback = move |source, action, key, value| {
        if let Some(strong) = weak2.upgrade() {
          WebcamNode::on_state(&strong, source, action, key, value);
        }
      };
      let _state_connection = state.connect(callback);
      RefCell::new(Self {
        api,
        _state_connection,
        main_thread_token,
        state: state.clone(),
        signaler,
        webcam_thread: None,
        active_formats: Arc::new(Mutex::new(None)),
        viewer: None,
        viewer_sink: ImageSink::new(),
      })
    });

    let weak = Rc::downgrade(&result);
    node_token.set_process(move |handle, request| {
      if let Some(strong) = weak.upgrade() {
        strong.borrow_mut().process(handle, request);
      }
    });

    state.recap();
    result
  }
}

impl Drop for WebcamNode {
  fn drop(&mut self) {
    self.viewer = None;
    self.stop_webcam();
    self.state.set(
      api::StateKey::String("Running".to_string()),
      api::StateValue::Bool(false),
    );
  }
}
