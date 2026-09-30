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
use crate::audio_converter::{AudioConverterParams, AudioFormat};
use crate::image_converter::ConverterParams;
use crate::media_converter::{MediaConverter, MediaConverterParams};
use crate::image_viewer::{ImageSink, ImageViewer};

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

/// Just the image half of a message, for forwarding it while its analog data
/// is converted.
struct ImageOnly<'a>(&'a dyn NodeData);

impl NodeData for ImageOnly<'_> {
  fn time(&self) -> Duration {
    self.0.time()
  }

  fn image(&self) -> Option<&dyn ImageData> {
    self.0.image()
  }
}

/// Just the analog half of a message, for forwarding it while its image is
/// converted, or when it has no image.
struct AnalogOnly<'a>(&'a dyn NodeData);

impl NodeData for AnalogOnly<'_> {
  fn time(&self) -> Duration {
    self.0.time()
  }

  fn analog(&self) -> Option<&dyn AnalogData> {
    self.0.analog()
  }
}

#[derive(Clone,Debug)]
struct ParamsHolder {
  params: MediaConverterParams,
  dirty: bool,
}

pub struct ImageConverterNode {
  api: ThalamusAPI,
  state: State,
  main_thread_token: MainThreadToken,
  // Open while View is true; shows every image this node outputs.
  viewer: Option<ImageViewer>,
  viewer_sink: ImageSink,
  params: Arc<Mutex<ParamsHolder>>,
  state_connection: Option<OnDrop>,
  source_connection: Option<OnDrop>,
  data_connection: Option<OnDrop>,
  signaler: Arc<OffMainSignaler>,
  converter: Arc<Mutex<MediaConverter>>,
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

    let mut this = rc.borrow_mut();
    match key_str.as_str() {
      "Format" => {
        let StateValue::String(v) = value else {
          return;
        };

        let mut lock = this.params.lock().unwrap();
        lock.dirty = true;
        lock.params.image.format = match v.to_uppercase().as_str() {
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
      "Audio Format" => {
        let StateValue::String(v) = value else {
          return;
        };

        let mut lock = this.params.lock().unwrap();
        lock.dirty = true;
        lock.params.audio.format = match v.to_uppercase().as_str() {
          "INTEGER" => Some(AudioFormat::Integer),
          "DECIMAL" => Some(AudioFormat::Decimal),
          "AAC" => Some(AudioFormat::AAC),
          _ => None
        };
      },
      "Audio Bitrate" => {
        // kbit/s for the whole stream; 0 means 64 kbit/s per channel.
        if let StateValue::Int(v) = value {
          let mut lock = this.params.lock().unwrap();
          lock.dirty = true;
          lock.params.audio.bitrate = if v > 0 { Some(v * 1000) } else { None };
        }
      },
      "Width" => {
        if let StateValue::Int(v) = value {
          let mut lock = this.params.lock().unwrap();
          lock.dirty = true;
          lock.params.image.width = if v > 0 { Some(v as i32) } else { None };
        }
      },
      "Height" => {
        if let StateValue::Int(v) = value {
          let mut lock = this.params.lock().unwrap();
          lock.dirty = true;
          lock.params.image.height = if v > 0 { Some(v as i32) } else { None };
        }
      },
      "Quality" => {
        if let StateValue::Int(v) = value {
          let mut lock = this.params.lock().unwrap();
          lock.dirty = true;
          lock.params.image.quality = if v > 0 { Some(v as i32) } else { None };
        }
      },
      "View" => {
        if value != StateValue::Bool(true) {
          this.viewer = None;
        } else if this.viewer.is_none() {
          let (api, state, sink, token) =
            (this.api, this.state.clone(), this.viewer_sink.clone(), this.main_thread_token);
          // ImageViewer::new may write view_geometry to the state, which calls
          // back into on_state, so the node can't stay borrowed.
          drop(this);
          match ImageViewer::new(api, state, &sink, token) {
            Ok(viewer) => rc.borrow_mut().viewer = Some(viewer),
            Err(e) => println!("ImageConverterNode: failed to create image viewer: {e}"),
          }
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
          let viewer_sink = borrow.viewer_sink.clone();
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
            // Latency is only measured for images.
            if data.image().is_some() {
              record_arrival(&arrivals, data.time(), arrived);
            }
            let image_needs = converter.needs_image_conversion(&data);
            let audio_needs = converter.needs_audio_conversion(&data);
            if image_needs || audio_needs {
              converter.push(&data);
              notify.notify_one();
            }

            // Forward whatever isn't being converted. Images carry the stats
            // channels, which replace the message's own analog data, so
            // analog data is forwarded with an image only when neither half
            // is converted (as before audio conversion existed).
            let forward_image = data.image().is_some() && !image_needs;
            let forward_analog = data.analog().is_some() && !audio_needs;
            if forward_image {
              let latency_ms = take_latency_ms(&arrivals, data.time(), api.time());
              if let Some(image) = data.image() {
                viewer_sink.update(image);
              }
              if audio_needs {
                let _ = signaler.ready_this_thread(&WithStats::new(&ImageOnly(&data), latency_ms));
              } else {
                let _ = signaler.ready_this_thread(&WithStats::new(&data, latency_ms));
              }
            } else if forward_analog {
              let _ = signaler.ready_this_thread(&AnalogOnly(&data));
            }
          }));
        }));
        rc.borrow_mut().source_connection = temp;
      }
      _ => {}
    }
  }

  async fn converter_task(
    converter: Arc<Mutex<MediaConverter>>,
    signaler: Arc<OffMainSignaler>,
    notify: Arc<Notify>,
    dropping: Arc<AtomicBool>,
    api: ThalamusAPIThreadSafe,
    arrivals: Arrivals,
    viewer_sink: ImageSink,
  ) {
    loop {
      if dropping.load(Ordering::SeqCst) {
        return;
      }
      {
        let mut converter = converter.lock().unwrap();
        while let Some(output) = converter.pull() {
          let Some(image) = output.image() else {
            // Converted audio has no stats channels (a message has one
            // analog sample type, and the stats are f64).
            let _ = signaler.ready(&*output);
            continue;
          };
          // Converted images keep their input's time, which is the key.
          let latency_ms = take_latency_ms(&arrivals, output.time(), api.time());
          // Encoded (MPEG4) output is dropped by the viewer.
          viewer_sink.update(image);
          let _ = signaler.ready(&WithStats::new(&*output, latency_ms));
        }
      }
      notify.notified().await;
    }
  }
}

impl Node for ImageConverterNode {
  fn new(api: ThalamusAPI, node_token: NodeToken, state: State, token: MainThreadToken) -> Rc<RefCell<Self>> {
    let signaler = OffMainSignaler::new(api, node_token);
    signaler.unblock();
    let params = ParamsHolder {
      params: MediaConverterParams {
        image: ConverterParams {
          format: None,
          width: None,
          height: None,
          quality: None,
        },
        audio: AudioConverterParams::default(),
      },
      dirty: true,
    };
    let result = Rc::new(RefCell::new(ImageConverterNode {
      params: Arc::new(Mutex::new(params.clone())),
      converter: Arc::new(Mutex::new(MediaConverter::new(api.thread_safe(), params.params))),
      api,
      state: state.clone(),
      main_thread_token: token,
      viewer: None,
      viewer_sink: ImageSink::new(),
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
      let viewer_sink = borrow.viewer_sink.clone();
      api.tokio().as_ref().unwrap().spawn(ImageConverterNode::converter_task(
        converter,
        signaler,
        notify,
        dropping,
        api.thread_safe(),
        arrivals,
        viewer_sink,
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
