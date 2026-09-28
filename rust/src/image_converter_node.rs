use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use crate::api::{
  AnalogData, ImageData, ImageFormat,
  MainThreadToken, Node, NodeConsts, NodeData, NodeSelector, NodeToken, OffMainSignaler, OnDrop, State, StateAction, StateValue, THALAMUS_MODALITY_ANALOG, THALAMUS_MODALITY_IMAGE, ThalamusAPI,
  ThalamusAPIThreadSafe,
};
use crate::image_converter::{Converter, ConverterParams};

/// Input image time (NodeData::time) -> when that image arrived, for images
/// that haven't come out of the node yet.
type Arrivals = Arc<Mutex<HashMap<Duration, Duration>>>;

/// Images the converter drops (e.g. on reconfiguration or a decode error)
/// never come out, so once this many are tracked, entries older than
/// STALE_ARRIVAL are forgotten.
const MAX_TRACKED_ARRIVALS: usize = 256;
const STALE_ARRIVAL: Duration = Duration::from_secs(10);

fn record_arrival(arrivals: &Arrivals, time: Duration, now: Duration) {
  let mut arrivals = arrivals.lock().unwrap();
  if arrivals.len() >= MAX_TRACKED_ARRIVALS {
    arrivals.retain(|_, arrived| now.saturating_sub(*arrived) < STALE_ARRIVAL);
  }
  arrivals.insert(time, now);
}

/// Milliseconds since the image with input time `time` arrived, if it was
/// recorded; forgets it either way.
fn take_latency_ms(arrivals: &Arrivals, time: Duration, now: Duration) -> Option<f64> {
  let arrived = arrivals.lock().unwrap().remove(&time)?;
  Some(now.saturating_sub(arrived).as_secs_f64() * 1000.0)
}

/// An outgoing image plus a two-channel analog signal: its conversion latency
/// (no sample when it isn't known) and the number of bytes in its planes.
struct WithStats<'a> {
  inner: &'a dyn NodeData,
  latency_ms: Option<f64>,
  output_bytes: f64,
}

impl<'a> WithStats<'a> {
  fn new(inner: &'a dyn NodeData, latency_ms: Option<f64>) -> Self {
    let output_bytes = inner.image().map_or(0, |image| {
      (0..image.num_planes()).map(|i| image.plane(i as i32).len()).sum::<usize>()
    });
    WithStats { inner, latency_ms, output_bytes: output_bytes as f64 }
  }
}

impl NodeData for WithStats<'_> {
  fn time(&self) -> Duration {
    self.inner.time()
  }

  fn image(&self) -> Option<&dyn ImageData> {
    self.inner.image()
  }

  fn analog(&self) -> Option<&dyn AnalogData> {
    Some(self)
  }
}

impl AnalogData for WithStats<'_> {
  fn data(&self, channel: i32) -> &[f64] {
    match channel {
      0 => self.latency_ms.as_slice(),
      1 => std::slice::from_ref(&self.output_bytes),
      _ => &[],
    }
  }

  fn num_channels(&self) -> i32 {
    2
  }

  fn sample_interval(&self, _channel: i32) -> Duration {
    Duration::ZERO
  }

  fn name(&self, channel: i32) -> &str {
    match channel {
      0 => "Latency (ms)",
      1 => "Output Bytes",
      _ => "",
    }
  }
}

#[derive(Clone,Debug)]
struct ParamsHolder {
  params: ConverterParams,
  dirty: bool,
}

pub struct ImageConverterNode {
  api: ThalamusAPI,
  params: Arc<Mutex<ParamsHolder>>,
  state_connection: Option<OnDrop>,
  source_connection: Option<OnDrop>,
  data_connection: Option<OnDrop>,
  signaler: Arc<OffMainSignaler>,
  converter: Arc<Mutex<Converter>>,
  notify: Arc<Notify>,
  dropping: Arc<AtomicBool>,
  arrivals: Arrivals,
}

impl NodeConsts for ImageConverterNode {
  const MODALITIES: u32 = THALAMUS_MODALITY_IMAGE | THALAMUS_MODALITY_ANALOG;
  const SIGNALS_OFFMAIN: bool = true;
}

impl ImageConverterNode {
  fn on_state(rc: Rc<RefCell<Self>>, _source: State, _action: StateAction, key: StateValue, value: StateValue) {
    let StateValue::String(key_str) = key else {
      return;
    };

    let this = rc.borrow_mut();
    match key_str.as_str() {
      "Format" => {
        let StateValue::String(v) = value else {
          return;
        };

        let mut lock = this.params.lock().unwrap();
        lock.dirty = true;
        lock.params.format = match v.to_uppercase().as_str() {
          "GRAY" => Some(ImageFormat::Gray),
          "RGB" => Some(ImageFormat::RGB), 
          "YUYV422" => Some(ImageFormat::YUYV422), 
          "YUV420P" => Some(ImageFormat::YUV420P), 
          "YUVJ420P" => Some(ImageFormat::YUVJ420P), 
          "NV12" => Some(ImageFormat::NV12), 
          "BGR" => Some(ImageFormat::BGR), 
          "MPEG4" => Some(ImageFormat::MPEG4),
          _ => None
        };
      },
      "Width" => {
        if let StateValue::Int(v) = value {
          let mut lock = this.params.lock().unwrap();
          lock.dirty = true;
          lock.params.width = if v > 0 { Some(v as i32) } else { None };
        }
      },
      "Height" => {
        if let StateValue::Int(v) = value {
          let mut lock = this.params.lock().unwrap();
          lock.dirty = true;
          lock.params.height = if v > 0 { Some(v as i32) } else { None };
        }
      },
      "Quality" => {
        if let StateValue::Int(v) = value {
          let mut lock = this.params.lock().unwrap();
          lock.dirty = true;
          lock.params.quality = if v > 0 { Some(v as i32) } else { None };
        }
      },
      "Source" => {
        let StateValue::String(name) = value else {
          return;
        };

        let weak = Rc::downgrade(&rc);
        let api = this.api;
        drop(this);

        let temp  = Some(api.get_node(NodeSelector::Name(name), move |node| {
          let Some(this) = weak.upgrade() else {
            return
          };
          let mut borrow = this.borrow_mut();
          let params = borrow.params.clone();
          let converter = borrow.converter.clone();
          let signaler = borrow.signaler.clone();
          let notify = borrow.notify.clone();
          let arrivals = borrow.arrivals.clone();
          let api = borrow.api.thread_safe();
          borrow.data_connection = Some(node.subscribe_multithreaded(move |node| {
            let arrived = api.time();
            let mut converter = converter.lock().unwrap();
            {
              let mut params = params.lock().unwrap();
              if params.dirty {
                converter.reconfigure(params.params);
                params.dirty = false;
              }
            }

            let data = node.data();
            record_arrival(&arrivals, data.time(), arrived);
            if !converter.needs_conversion(&data) {
              let latency_ms = take_latency_ms(&arrivals, data.time(), api.time());
              let _ = signaler.ready_this_thread(&WithStats::new(&data, latency_ms));
            } else {
              converter.push(&data);
              notify.notify_one();
            }
          }));
        }));
        rc.borrow_mut().source_connection = temp;
      }
      _ => {}
    }
  }

  async fn converter_task(
    converter: Arc<Mutex<Converter>>,
    signaler: Arc<OffMainSignaler>,
    notify: Arc<Notify>,
    dropping: Arc<AtomicBool>,
    api: ThalamusAPIThreadSafe,
    arrivals: Arrivals,
  ) {
    loop {
      if dropping.load(Ordering::SeqCst) {
        return;
      }
      {
        let mut converter = converter.lock().unwrap();
        while let Some(image) = converter.pull() {
          // Converted images keep their input's time, which is the key.
          let latency_ms = take_latency_ms(&arrivals, image.time(), api.time());
          let _ = signaler.ready(&WithStats::new(&*image, latency_ms));
        }
      }
      notify.notified().await;
    }
  }
}

impl Node for ImageConverterNode {
  fn new(api: ThalamusAPI, node_token: NodeToken, state: State, _token: MainThreadToken) -> Rc<RefCell<Self>> {
    let signaler = OffMainSignaler::new(api, node_token);
    signaler.unblock();
    let params = ParamsHolder {
      params: ConverterParams { 
        format: None, 
        width: None, 
        height: None, 
        quality: None,
      },
      dirty: true,
    };
    let result = Rc::new(RefCell::new(ImageConverterNode {
      params: Arc::new(Mutex::new(params.clone())),
      converter: Arc::new(Mutex::new(Converter::new(api.thread_safe(), params.params))),
      api,
      state_connection: None,
      source_connection: None,
      data_connection: None,
      signaler,
      notify: Arc::new(Notify::new()),
      dropping: Arc::new(AtomicBool::new(false)),
      arrivals: Arc::new(Mutex::new(HashMap::new())),
    }));

    let change_ref = Rc::downgrade(&result);
    let state_callback =
      move |s, a, k, v| {
        if let Some(lock) = change_ref.upgrade() {
          ImageConverterNode::on_state(lock, s, a, k, v);
        };
      };

    result.borrow_mut().state_connection = Some(state.connect(state_callback));
    state.recap();

    {
      let borrow = result.borrow();
      let converter = borrow.converter.clone();
      let signaler = borrow.signaler.clone();
      let notify = borrow.notify.clone();
      let dropping = borrow.dropping.clone();
      let arrivals = borrow.arrivals.clone();
      api.tokio().as_ref().unwrap().spawn(ImageConverterNode::converter_task(
        converter,
        signaler,
        notify,
        dropping,
        api.thread_safe(),
        arrivals,
      ));
    }
    result
  }

  fn predrop(&self, token: crate::api::PredropToken) {
    self.dropping.store(true, Ordering::SeqCst);
    self.signaler.predrop(token);
    self.notify.notify_one();
  }
}
