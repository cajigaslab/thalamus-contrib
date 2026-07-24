use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, TrySendError, sync_channel};

use crate::api::{
  Json, MainThreadOnly, MainThreadToken, Node, NodeData, NodeSelector, NodeToken, OnDrop,
  PredropToken, Request, State, StateAction, StateKey, StateValue, ThalamusAPI,
  ThalamusAPIThreadSafe,
};
use crate::rtmps_publisher::{FrameBuf, RtmpsPublisher};

fn state_get_string(state: &State, key: &str) -> Option<String> {
  match state.get(StateKey::String(key.to_string())) {
    Some(StateValue::String(s)) if !s.is_empty() => Some(s),
    _ => None,
  }
}

struct RtmpsNodeInner {
  api: ThalamusAPI,
  state: State,
  state_connection: Option<OnDrop>,
  // Connected to the node named by "Source"; re-fires (rewiring
  // data_connection below) if the node graph changes which node that name
  // resolves to.
  source_connection: Option<OnDrop>,
  // Connected to the currently resolved source node's frame-ready signal.
  data_connection: Option<OnDrop>,
  frame_tx: Option<std::sync::mpsc::SyncSender<FrameBuf>>,
  publish_thread: Option<std::thread::JoinHandle<()>>,
  main_thread_token: MainThreadToken,
}

pub struct RtmpsNode {
  inner: Rc<RefCell<RtmpsNodeInner>>,
}

fn start_publishing(inner: &Rc<RefCell<RtmpsNodeInner>>) {
  let (api, main_thread_token, source, destination) = {
    let borrow = inner.borrow();
    (
      borrow.api,
      borrow.main_thread_token,
      state_get_string(&borrow.state, "Source"),
      state_get_string(&borrow.state, "Destination"),
    )
  };

  let Some(source) = source else {
    println!("RtmpsNode: no Source set");
    return;
  };
  let Some(destination) = destination else {
    println!("RtmpsNode: no Destination set");
    return;
  };

  // Bounded so a stalled/slow network write applies backpressure by
  // dropping frames instead of growing memory without bound -- the
  // publisher thread is the only consumer, so a few frames of slack is
  // enough to smooth over momentary jitter.
  let (frame_tx, frame_rx) = sync_channel::<FrameBuf>(4);

  // Lets the publisher thread report a fatal error back to the node despite
  // Rc<RefCell<..>> not being Send: wrapped once here (on the main thread,
  // proven by main_thread_token), the wrapper itself is Send and can only be
  // unwrapped again via a MainThreadToken obtained from a later post_to_main
  // callback -- same pattern ThalamusAPI::join_then uses.
  let mt_api = api.thread_safe();
  let inner_for_error = MainThreadOnly::new(Rc::clone(inner), main_thread_token);
  let publish_thread = std::thread::spawn(move || {
    run_publisher(frame_rx, destination, mt_api, inner_for_error);
  });

  let inner_for_source = Rc::clone(inner);
  let frame_tx_for_source = frame_tx.clone();
  let source_connection = api.get_node(NodeSelector::Name(source), move |ext_node| {
    // A node now matches "Source" -- either the first resolution, or the
    // previously matched node was replaced. Rewire frame delivery; the
    // publisher thread/pipeline itself stays open across this.
    //
    // Captured directly (rather than read back out of inner.frame_tx) since
    // get_node's callback can fire synchronously before this function has
    // stored frame_tx into inner below.
    let frame_tx = frame_tx_for_source.clone();
    let data_connection = ext_node.subscribe_multithreaded(move |ext_node| {
      let data = ext_node.data();
      let Some(image) = data.image() else {
        return;
      };
      let format = image.format();
      let width = image.width();
      let height = image.height();
      let planes = (0..image.num_planes())
        .map(|i| image.plane(i as i32).to_vec())
        .collect();
      let frame = FrameBuf {
        format,
        width: width as u32,
        height: height as u32,
        planes,
        timestamp: data.time(),
        frame_interval: image.frame_interval(),
      };
      if let Err(TrySendError::Full(_)) = frame_tx.try_send(frame) {
        println!("RtmpsNode: dropping frame, publisher is falling behind");
      }
    });
    inner_for_source.borrow_mut().data_connection = Some(data_connection);
  });

  let mut borrow = inner.borrow_mut();
  borrow.source_connection = Some(source_connection);
  borrow.frame_tx = Some(frame_tx);
  borrow.publish_thread = Some(publish_thread);
}

fn run_publisher(
  frame_rx: Receiver<FrameBuf>,
  destination: String,
  mt_api: ThalamusAPIThreadSafe,
  inner: MainThreadOnly<Rc<RefCell<RtmpsNodeInner>>>,
) {
  let mut publisher: Option<RtmpsPublisher> = None;
  let mut error = None;

  for frame in frame_rx {
    let pub_ref = match publisher.as_mut() {
      Some(p) => p,
      None => match RtmpsPublisher::open(
        &destination,
        frame.format,
        frame.width,
        frame.height,
        frame.frame_interval,
      ) {
        Ok(p) => publisher.insert(p),
        Err(e) => {
          error = Some(format!("failed to open {}: {}", destination, e));
          break;
        }
      },
    };
    if let Err(e) = pub_ref.write_frame(&frame) {
      error = Some(e);
      break;
    }
  }

  if let Some(e) = error {
    println!("RtmpsNode: {}, stopping", e);
    // Flips Running to false, which the node's state-change handler picks
    // up to tear down the source/data subscriptions and join this thread
    // (from a threadpool thread, not this one -- see stop_publishing).
    mt_api.post_to_main(move |token| {
      let inner = inner.take(token);
      let running = StateKey::String("Running".to_string());
      inner.borrow().state.set(running, StateValue::Bool(false));
    });
  }

  println!("RtmpsNode: publisher thread exited");
}

fn stop_publishing<F: FnOnce() + 'static>(inner: &Rc<RefCell<RtmpsNodeInner>>, on_stopped: F) {
  let (handle, api, main_thread_token) = {
    let mut borrow = inner.borrow_mut();
    borrow.source_connection = None; // stop resolving/re-resolving "Source"
    borrow.data_connection = None; // stop receiving frames
    borrow.frame_tx = None; // closes the channel, ending the publisher thread's loop
    (
      borrow.publish_thread.take(),
      borrow.api,
      borrow.main_thread_token,
    )
  };
  // Never join on the main thread: closing the RTMPS connection can block on
  // network I/O. join_then hands the join off to a threadpool thread and
  // runs on_stopped once back on the main thread.
  api.join_then(handle, main_thread_token, on_stopped);
}

impl Node for RtmpsNode {
  fn process(&self, handle: Request, _request: Json) {
    let api = self.inner.borrow().api;
    handle.respond(&Json::from_string(api, "null"));
  }

  fn new(api: ThalamusAPI, _node_token: NodeToken, state: State, token: MainThreadToken) -> Self {
    let inner = Rc::new(RefCell::new(RtmpsNodeInner {
      api,
      state: state.clone(),
      state_connection: None,
      source_connection: None,
      data_connection: None,
      frame_tx: None,
      publish_thread: None,
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
              start_publishing(&change_ref);
            } else {
              stop_publishing(&change_ref, || {});
            }
          }
          _ => {}
        }
      };

    inner.borrow_mut().state_connection = Some(state.connect(state_callback));
    state.recap();

    RtmpsNode { inner }
  }

  fn predrop(&self, token: PredropToken) {
    stop_publishing(&self.inner, move || {
      token.ready();
    });
  }
}

impl Drop for RtmpsNode {
  fn drop(&mut self) {
    stop_publishing(&self.inner, || {});
  }
}
