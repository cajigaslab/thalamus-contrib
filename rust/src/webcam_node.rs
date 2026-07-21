use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use crate::api::{
  Json, MainThreadToken, Node, NodeToken, OnDrop, PredropToken, Request, State, StateAction,
  StateKey, StateValue, THALAMUS_MODALITY_IMAGE, ThalamusAPI, ThalamusAPIThreadSafe,
};
use crate::ffmpeg_devices::{CameraInfo, WebcamCapture, list_cameras, list_formats, probe_resolution};

static CAMERAS: OnceLock<Vec<CameraInfo>> = OnceLock::new();

const DEFAULT_WIDTH: u32 = 640;
const DEFAULT_HEIGHT: u32 = 480;

fn selected_device_name(state: &State) -> Option<String> {
  let selected = state.get(StateKey::String("Camera".to_string()));
  match selected {
    Some(StateValue::String(description)) => CAMERAS
      .get()
      .and_then(|cameras| cameras.iter().find(|c| c.description == description))
      .map(|c| c.device_name.clone()),
    _ => None,
  }
}

fn state_get_u32(state: &State, key: &str) -> Option<u32> {
  match state.get(StateKey::String(key.to_string())) {
    Some(StateValue::Int(i)) if i > 0 => Some(i as u32),
    _ => None,
  }
}

struct WebcamNodeInner {
  api: ThalamusAPI,
  node_token: NodeToken,
  state: State,
  state_connection: Option<OnDrop>,
  capture_thread: Option<std::thread::JoinHandle<()>>,
  stop_flag: Arc<AtomicBool>,
  main_thread_token: MainThreadToken,
}

pub struct WebcamNode {
  inner: Rc<RefCell<WebcamNodeInner>>,
}

fn start_capture(inner: &Rc<RefCell<WebcamNodeInner>>) {
  let inner_clone = Rc::clone(inner);
  // Mirrors ThorcamNode: don't spawn the new capture thread until the old
  // one (if any) has fully exited, so a stale thread can't still be mid
  // shutdown (e.g. holding the device open) when a new one tries to start.
  stop_capture(inner, move || {
    start_capture_impl(&inner_clone);
  });
}

fn start_capture_impl(inner: &Rc<RefCell<WebcamNodeInner>>) {
  let (api, node_token, device_name, width, height) = {
    let borrow = inner.borrow();
    let device_name = selected_device_name(&borrow.state);
    let width = state_get_u32(&borrow.state, "Width").unwrap_or(DEFAULT_WIDTH);
    let height = state_get_u32(&borrow.state, "Height").unwrap_or(DEFAULT_HEIGHT);
    (borrow.api, borrow.node_token.clone(), device_name, width, height)
  };

  let Some(device_name) = device_name else {
    println!("WebcamNode: no camera selected");
    return;
  };

  let stop_flag = Arc::new(AtomicBool::new(false));
  let stop_clone = Arc::clone(&stop_flag);

  let mt_api = api.thread_safe();
  let handle = std::thread::spawn(move || {
    run_capture(mt_api, node_token, stop_clone, device_name, width, height);
  });

  let mut borrow = inner.borrow_mut();
  borrow.stop_flag = stop_flag;
  borrow.capture_thread = Some(handle);
}

fn stop_capture<F: FnOnce() + 'static>(inner: &Rc<RefCell<WebcamNodeInner>>, on_stopped: F) {
  let (handle, api, main_thread_token) = {
    let mut borrow = inner.borrow_mut();
    borrow.stop_flag.store(true, Ordering::Relaxed);
    (
      borrow.capture_thread.take(),
      borrow.api,
      borrow.main_thread_token,
    )
  };
  // Never join on the main thread (see ThorcamNode) -- join_then hands the
  // join off to a threadpool thread and runs on_stopped once back on the
  // main thread.
  api.join_then(handle, main_thread_token, on_stopped);
}

fn run_capture(
  api: ThalamusAPIThreadSafe,
  node_token: NodeToken,
  stop: Arc<AtomicBool>,
  device_name: String,
  width: u32,
  height: u32,
) {
  let mut capture = match WebcamCapture::open(&device_name, width, height) {
    Ok(capture) => capture,
    Err(e) => {
      println!("WebcamNode: failed to open {}: {}", device_name, e);
      return;
    }
  };

  while !stop.load(Ordering::Relaxed) {
    let Some(frame) = capture.read_frame() else {
      println!("WebcamNode: {} stopped producing frames", device_name);
      break;
    };
    let frame = frame.with_time(crate::api::time(api.raw));
    // Publishes synchronously from this thread; subscribers read plane()
    // before this call returns. Ignore if the node was destroyed
    // concurrently.
    let _ = api.ready_offmain(&frame, &node_token);
  }

  println!("WebcamNode: capture thread exited");
}

impl Node for WebcamNode {
  fn modalities(&self) -> u32 {
    THALAMUS_MODALITY_IMAGE
  }

  fn process(&self, handle: Request, request: Json) {
    let api = self.inner.borrow().api;
    let response = match serde_json::from_str::<serde_json::Value>(&request.to_string()) {
      Ok(serde_json::Value::String(s)) if s == "get_cameras" => {
        let cameras: Vec<serde_json::Value> = CAMERAS
          .get_or_init(list_cameras)
          .iter()
          .map(|c| serde_json::Value::String(c.description.clone()))
          .collect();
        serde_json::to_string_pretty(&cameras).unwrap()
      }
      Ok(serde_json::Value::String(s)) if s == "test_resolution" => {
        let borrow = self.inner.borrow();
        let device_name = selected_device_name(&borrow.state);
        let width = state_get_u32(&borrow.state, "Width");
        let height = state_get_u32(&borrow.state, "Height");
        drop(borrow);

        let formats = device_name.as_ref().and_then(|name| match list_formats(name) {
          Ok(formats) => Some(formats),
          Err(e) => {
            println!("WebcamNode: could not list formats for {}: {}", name, e);
            None
          }
        });

        let result = match (&device_name, width, height) {
          (None, _, _) => Err("no camera selected".to_string()),
          (_, None, _) | (_, _, None) => Err("width and height must both be set".to_string()),
          (Some(device_name), Some(width), Some(height)) => {
            probe_resolution(device_name, width as i32, height as i32)
          }
        };

        let mut response = match result {
          Ok(()) => serde_json::json!({ "success": true }),
          Err(error) => serde_json::json!({ "success": false, "error": error }),
        };
        if let Some(formats) = formats {
          response["formats"] = serde_json::Value::String(formats);
        }
        response.to_string()
      }
      _ => "null".to_string(),
    };
    println!("{}", response);
    handle.respond(&Json::from_string(api, &response));
  }

  fn new(api: ThalamusAPI, node_token: NodeToken, state: State, token: MainThreadToken) -> Self {
    CAMERAS.get_or_init(list_cameras);

    let inner = Rc::new(RefCell::new(WebcamNodeInner {
      api,
      node_token,
      state: state.clone(),
      state_connection: None,
      capture_thread: None,
      stop_flag: Arc::new(AtomicBool::new(false)),
      main_thread_token: token,
    }));

    let change_ref = Rc::clone(&inner);
    let state_callback =
      move |_source: State, _action: StateAction, key: StateValue, value: StateValue| {
        let StateValue::String(key_str) = key else {
          return;
        };
        match key_str.as_str() {
          "Running" => {
            if value == StateValue::Bool(true) {
              start_capture(&change_ref);
            } else {
              stop_capture(&change_ref, || {});
            }
          }
          _ => {}
        }
      };

    inner.borrow_mut().state_connection = Some(state.connect(state_callback));
    state.recap();

    WebcamNode { inner }
  }

  fn predrop(&self, token: PredropToken) {
    stop_capture(&self.inner, move || {
      token.ready();
    });
  }
}

impl Drop for WebcamNode {
  fn drop(&mut self) {
    stop_capture(&self.inner, || {});
  }
}
