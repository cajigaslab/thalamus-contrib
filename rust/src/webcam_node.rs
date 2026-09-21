use std::{sync::{Arc, Mutex}, time::Duration};
use nokhwa::pixel_format::RgbFormat;

use crate::api::{
  self, ImageData, Json, MainThreadOnly, MainThreadToken, Node, NodeData, NodeToken, OffMainSignaler, OnDrop, Request, State, StateAction, StateValue, THALAMUS_MODALITY_IMAGE, ThalamusAPI, ThalamusAPIThreadSafe
};

struct Frame<'a> {
  buffer: &'a [u8],
  width: u64,
  height: u64,
  format: api::ImageFormat,
  time: Duration,
  frame_interval: Duration,
}

impl<'a> NodeData for Frame<'a> {
  fn analog(&self) -> Option<&dyn api::AnalogData> { None }

  fn image(&self) -> Option<&dyn ImageData> { Some(self) }

  fn mocap(&self) -> Option<&dyn api::MocapData> { None }

  fn text(&self) -> Option<&dyn api::TextData> { None }
  
  fn time(&self) -> Duration {
    self.time
  }
}

impl<'a> ImageData for Frame<'a> {
  fn plane(
            &self,
            _channel: i32,
        ) -> &[u8] {
    self.buffer
  }

  fn num_planes(&self) -> u64 {
    1
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

struct WebcamNodeInner {
  api:              ThalamusAPI,
  _state_connection: OnDrop,
  main_thread_token: MainThreadToken,
  state: State,
  signaler: Arc<OffMainSignaler>,
  webcam_thread: Option<std::thread::JoinHandle<()>>,
}

pub struct WebcamNode {
    inner: Arc<Mutex<WebcamNodeInner>>,
}

impl WebcamNodeInner {
  fn stop_webcam(&mut self) {
    let _ = self.signaler.block();
    self.webcam_thread.take().map(|h| {
      h.join()
    });
  }

  fn webcam(api: ThalamusAPIThreadSafe, signaler: Arc<OffMainSignaler>) {
    let index = nokhwa::utils::CameraIndex::Index(0);
    let format = nokhwa::utils::RequestedFormat::new::<RgbFormat>(nokhwa::utils::RequestedFormatType::AbsoluteHighestResolution);
    let mut camera =  match nokhwa::Camera::new(index, format) {
      Ok(c) => {
        c
      },
      Err(e) => {
        println!("Camera Selection failed: {:?}", e);
        return;
      }
    };
    match camera.open_stream() {
      Ok(_) => {},
      Err(e) => {
        println!("Camera Open failed: {:?}", e);
        return;
      }
    };

    loop {
      let buffer = match camera.frame() {
        Ok(v) => {v},
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
      let frame_interval = Duration::from_secs_f64(1.0/(camera.frame_rate() as f64));

      let resolution = buffer.resolution();
      //println!("{} {} {} {}", buffer.buffer().len(), resolution.width(), resolution.height(), buffer.source_frame_format());
      let frame = Frame {
        buffer: buffer.buffer(),
        width: resolution.width() as u64,
        height: resolution.height() as u64,
        time, format, frame_interval
      };
      match signaler.ready(&frame) {
        Ok(v) => { 
          if !v {
            println!("Blocked");
            break
          } 
        },
        Err(_) => {break}
      }
    }
  }

  fn on_state(me: Arc<Mutex<WebcamNodeInner>>, _source: State, _action: StateAction, key: StateValue, value: StateValue) {
    let StateValue::String(key_str) = key else {
      return;
    };
    match key_str.as_str() {
      "Running" => {
        me.lock().unwrap().stop_webcam();
        if value == StateValue::Bool(true) {
          let mut lock = me.lock().unwrap();
          let api = lock.api.thread_safe();
          let signaler = lock.signaler.clone();
          let _ = signaler.unblock();
          let wrapped_state = MainThreadOnly::new(lock.state.clone(), lock.main_thread_token);
          lock.webcam_thread = Some(std::thread::spawn(move || {
            WebcamNodeInner::webcam(api, signaler);
            api.post_to_main(|main_thread_token| {
                let state = wrapped_state.take(main_thread_token);
                state.set(api::StateKey::String("Running".to_string()), api::StateValue::Bool(false));
            });
          }));
        }
      }
      _ => {}
    }
  }
}

impl Node for WebcamNode {
  fn modalities(&self) -> u32 {
    THALAMUS_MODALITY_IMAGE
  }

  fn signals_offmain(&self) -> bool {
    true
  }

  fn process(&self, handle: Request, _request: Json) {
    let api = self.inner.lock().unwrap().api;
    handle.respond(&Json::from_string(api, "null"));
  }

  fn new(api: ThalamusAPI, node_token: NodeToken, state: State, main_thread_token: MainThreadToken) -> Self {
    let inner = Arc::new_cyclic(|weak: &std::sync::Weak<Mutex<WebcamNodeInner>>| {
      let weak2 = weak.clone();
      let state_callback = move |source: State, action: StateAction, key: StateValue, value: StateValue| {
        if let Some(strong) = weak2.upgrade() {
          WebcamNodeInner::on_state(strong, source, action, key, value);
        }
      };

      let _state_connection = state.connect(state_callback);
      let signaler = Arc::new(OffMainSignaler::new(api, node_token.clone()));
      Mutex::new(WebcamNodeInner {
        api, _state_connection, main_thread_token,
        state: state.clone(),
        signaler, webcam_thread: None
      })
    });

    state.recap_with(|source: State, action: StateAction, key: StateValue, value: StateValue| {
      WebcamNodeInner::on_state(inner.clone(), source, action, key, value);
    });

    WebcamNode {
      inner
    }
  }
}

impl Drop for WebcamNode {
  fn drop(&mut self) {
    let mut lock = self.inner.lock().unwrap();
    lock.stop_webcam();
    lock.state.set(api::StateKey::String("Running".to_string()), api::StateValue::Bool(false));
  }
}
