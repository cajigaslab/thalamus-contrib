use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use crate::api::{
    Json, MainThreadToken, Node, NodeToken, OnDrop, PredropToken, Request, State, StateAction,
    StateKey, StateValue, ThalamusAPI, ThalamusAPIThreadSafe, THALAMUS_MODALITY_IMAGE,
};
use crate::ffmpeg_devices::{list_cameras, CameraInfo, WebcamCapture};

static CAMERAS: OnceLock<Vec<CameraInfo>> = OnceLock::new();

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
    let (api, node_token, device_name) = {
        let borrow = inner.borrow();
        let selected = borrow.state.get(StateKey::String("Camera".to_string()));
        let device_name = match selected {
            Some(StateValue::String(description)) => CAMERAS
                .get()
                .and_then(|cameras| cameras.iter().find(|c| c.description == description))
                .map(|c| c.device_name.clone()),
            _ => None,
        };
        (borrow.api, borrow.node_token.clone(), device_name)
    };

    let Some(device_name) = device_name else {
        println!("WebcamNode: no camera selected");
        return;
    };

    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop_flag);

    let mt_api = api.thread_safe();
    let handle = std::thread::spawn(move || {
        run_capture(mt_api, node_token, stop_clone, device_name);
    });

    let mut borrow = inner.borrow_mut();
    borrow.stop_flag = stop_flag;
    borrow.capture_thread = Some(handle);
}

fn stop_capture<F: FnOnce() + 'static>(inner: &Rc<RefCell<WebcamNodeInner>>, on_stopped: F) {
    let (handle, api, main_thread_token) = {
        let mut borrow = inner.borrow_mut();
        borrow.stop_flag.store(true, Ordering::Relaxed);
        (borrow.capture_thread.take(), borrow.api, borrow.main_thread_token)
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
) {
    let mut capture = match WebcamCapture::open(&device_name) {
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
        let state_callback = move |_source: State, _action: StateAction, key: StateValue, value: StateValue| {
            let StateValue::String(key_str) = key else { return };
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
