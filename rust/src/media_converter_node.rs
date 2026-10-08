use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use crate::api::{
  AnalogData, AnalogEncoding, AnalogFormat, ImageData, ImageFormat,
  MainThreadToken, Node, NodeConsts, NodeData, NodeSelector, NodeToken, OffMainSignaler, OnDrop, State, StateAction, StateValue, THALAMUS_MODALITY_ANALOG, THALAMUS_MODALITY_IMAGE, ThalamusAPI,
  ThalamusAPIThreadSafe,
};
use crate::audio_converter::{AudioConverterParams, AudioFormat};
use crate::image_converter::{ConverterParams, VideoFormat};
use crate::media_converter::{MediaConverter, MediaConverterParams};
use crate::image_viewer::{ImageSink, ImageViewer};

/// Input time (NodeData::time) -> when that input arrived, for inputs that
/// haven't come out of the node yet.
type ArrivalMap = Arc<Mutex<HashMap<Duration, Duration>>>;

/// Arrivals are tracked separately for images and analog data, since one
/// message can carry both with the same time.
#[derive(Clone, Default)]
struct Arrivals {
  image: ArrivalMap,
  analog: ArrivalMap,
}

/// Inputs the converter drops (e.g. on reconfiguration or a decode error) or
/// merges (several analog messages into one output) never come out under
/// their own time, so once this many are tracked, entries older than
/// STALE_ARRIVAL are forgotten.
const MAX_TRACKED_ARRIVALS: usize = 256;
const STALE_ARRIVAL: Duration = Duration::from_secs(10);

fn record_arrival(arrivals: &ArrivalMap, time: Duration, now: Duration) {
  let mut arrivals = arrivals.lock().unwrap();
  if arrivals.len() >= MAX_TRACKED_ARRIVALS {
    arrivals.retain(|_, arrived| now.saturating_sub(*arrived) < STALE_ARRIVAL);
  }
  arrivals.insert(time, now);
}

/// Milliseconds since the input with time `time` arrived, if it was
/// recorded; forgets it either way.
fn take_latency_ms(arrivals: &ArrivalMap, time: Duration, now: Duration) -> Option<f64> {
  let arrived = arrivals.lock().unwrap().remove(&time)?;
  Some(now.saturating_sub(arrived).as_secs_f64() * 1000.0)
}

/// The number of stats channels WithStats puts in front of an output's own
/// analog channels.
const STATS_CHANNELS: i32 = 2;

fn image_bytes(image: &dyn ImageData) -> usize {
  (0..image.num_planes()).map(|i| image.plane(i as i32).len()).sum()
}

/// Bytes of sample data plus any encoded buffer.
fn analog_bytes(analog: &dyn AnalogData) -> usize {
  let samples: usize = (0..analog.num_channels()).map(|channel| {
    let bytes_per_sample = match analog.analog_format(channel) {
      AnalogFormat::Double | AnalogFormat::ULong => 8,
      AnalogFormat::Int => 4,
      AnalogFormat::Short => 2,
      // Encoded samples are in the buffer.
      AnalogFormat::Encoded => 0,
    };
    bytes_per_sample * analog.count(channel)
  }).sum();
  samples + analog.buffer().len()
}

/// An outgoing image or audio message plus two stats channels: its
/// conversion latency (no sample when it isn't known) and its size in bytes.
/// The stats come before the message's own analog channels so those stay the
/// trailing run of channels, which is what a downstream converter reads.
struct WithStats<'a> {
  inner: &'a dyn NodeData,
  analog: Option<&'a dyn AnalogData>,
  latency_ms: Option<f64>,
  output_bytes: f64,
  /// Whether this message's channels differ from the previous output's; set
  /// by the converter task, which sees every output.
  channels_changed: bool,
}

/// A message's channels as subscribers see them, to tell when they change.
type ChannelLayout = Vec<(String, AnalogFormat, Duration)>;

fn channel_layout(analog: &dyn AnalogData) -> ChannelLayout {
  (0..analog.num_channels())
    .map(|c| (analog.name(c).to_string(), analog.analog_format(c), analog.sample_interval(c)))
    .collect()
}

impl<'a> WithStats<'a> {
  fn new(inner: &'a dyn NodeData, latency_ms: Option<f64>) -> Self {
    let analog = inner.analog();
    let output_bytes = inner.image().map_or(0, image_bytes) + analog.map_or(0, analog_bytes);
    WithStats { inner, analog, latency_ms, output_bytes: output_bytes as f64, channels_changed: false }
  }

  /// The inner analog data and its channel index for `channel`, if it isn't
  /// a stats channel.
  fn inner_channel(&self, channel: i32) -> Option<(&'a dyn AnalogData, i32)> {
    if channel < STATS_CHANNELS {
      return None;
    }
    self.analog.map(|analog| (analog, channel - STATS_CHANNELS))
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
      _ => self.inner_channel(channel).map_or(&[], |(a, c)| a.data(c)),
    }
  }

  fn short_data(&self, channel: i32) -> &[i16] {
    self.inner_channel(channel).map_or(&[], |(a, c)| a.short_data(c))
  }

  fn int_data(&self, channel: i32) -> &[i32] {
    self.inner_channel(channel).map_or(&[], |(a, c)| a.int_data(c))
  }

  fn ulong_data(&self, channel: i32) -> &[u64] {
    self.inner_channel(channel).map_or(&[], |(a, c)| a.ulong_data(c))
  }

  fn num_channels(&self) -> i32 {
    STATS_CHANNELS + self.analog.map_or(0, |a| a.num_channels())
  }

  fn sample_interval(&self, channel: i32) -> Duration {
    self.inner_channel(channel).map_or(Duration::ZERO, |(a, c)| a.sample_interval(c))
  }

  fn name(&self, channel: i32) -> &str {
    match channel {
      0 => "Latency (ms)",
      1 => "Output Bytes",
      _ => self.inner_channel(channel).map_or("", |(a, c)| a.name(c)),
    }
  }

  /// Formats are per channel: the stats are doubles whatever the audio is.
  fn analog_format(&self, channel: i32) -> AnalogFormat {
    self.inner_channel(channel).map_or(AnalogFormat::Double, |(a, c)| a.analog_format(c))
  }

  fn is_transformed(&self) -> bool {
    self.analog.is_some_and(|a| a.is_transformed())
  }

  fn buffer(&self) -> &[u8] {
    self.analog.map_or(&[], |a| a.buffer())
  }

  fn encoding(&self) -> AnalogEncoding {
    self.analog.map_or(AnalogEncoding::None, |a| a.encoding())
  }

  fn encoded_count(&self) -> u64 {
    self.analog.map_or(0, |a| a.encoded_count())
  }

  fn channels_changed(&self) -> bool {
    self.channels_changed
  }

  fn scale(&self, channel: i32) -> f64 {
    self.inner_channel(channel).map_or(1.0, |(a, c)| a.scale(c))
  }

  fn offset(&self, channel: i32) -> f64 {
    self.inner_channel(channel).map_or(0.0, |(a, c)| a.offset(c))
  }
}

#[derive(Clone,Debug)]
struct ParamsHolder {
  params: MediaConverterParams,
  dirty: bool,
}

pub struct MediaConverterNode {
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
  converter: Arc<MediaConverter>,
  notify: Arc<Notify>,
  dropping: Arc<AtomicBool>,
  arrivals: Arrivals,
}

impl NodeConsts for MediaConverterNode {
  const MODALITIES: u32 = THALAMUS_MODALITY_IMAGE | THALAMUS_MODALITY_ANALOG;
  const SIGNALS_OFFMAIN: bool = true;
}

impl MediaConverterNode {
  fn on_state(rc: Rc<RefCell<Self>>, _source: State, _action: StateAction, key: StateValue, value: StateValue) {
    let StateValue::String(key_str) = key else {
      return;
    };

    let mut this = rc.borrow_mut();
    match key_str.as_str() {
      "Video Format" => {
        let StateValue::String(v) = value else {
          return;
        };

        let mut lock = this.params.lock().unwrap();
        lock.dirty = true;
        // Compared case insensitively; anything else is passthrough.
        lock.params.image.format = match v.to_uppercase().as_str() {
          "DECODED" => Some(VideoFormat::Decoded),
          "GRAY" => Some(VideoFormat::Image(ImageFormat::Gray)),
          "RGB" => Some(VideoFormat::Image(ImageFormat::RGB)),
          "YUYV422" => Some(VideoFormat::Image(ImageFormat::YUYV422)),
          "YUV420P" => Some(VideoFormat::Image(ImageFormat::YUV420P)),
          "YUVJ420P" => Some(VideoFormat::Image(ImageFormat::YUVJ420P)),
          "NV12" => Some(VideoFormat::Image(ImageFormat::NV12)),
          "BGR" => Some(VideoFormat::Image(ImageFormat::BGR)),
          "MPEG4" => Some(VideoFormat::Image(ImageFormat::MPEG4)),
          "H264" => Some(VideoFormat::Image(ImageFormat::H264)),
          "VP9" => Some(VideoFormat::Image(ImageFormat::VP9)),
          _ => None
        };
      },
      "Audio Format" => {
        let StateValue::String(v) = value else {
          return;
        };

        let mut lock = this.params.lock().unwrap();
        lock.dirty = true;
        // Compared case insensitively; anything else is passthrough.
        lock.params.audio.format = match v.to_uppercase().as_str() {
          "DECODED" => Some(AudioFormat::Decoded),
          "INTEGER" => Some(AudioFormat::Integer),
          "DECIMAL" => Some(AudioFormat::Decimal),
          "AAC" => Some(AudioFormat::AAC),
          "OPUS" => Some(AudioFormat::Opus),
          _ => None
        };
      },
      "Audio Bit Rate" => {
        // kbit/s for the whole stream; 0 means 256 kbit/s.
        if let StateValue::Int(v) = value {
          let mut lock = this.params.lock().unwrap();
          lock.dirty = true;
          lock.params.audio.bitrate = if v > 0 { Some(v * 1000) } else { None };
        }
      },
      "Audio Sample Rate" => {
        // Hz; 0 keeps the source's sample rate.
        if let StateValue::Int(v) = value {
          let mut lock = this.params.lock().unwrap();
          lock.dirty = true;
          lock.params.audio.samplerate = if v > 0 { Some(v as i32) } else { None };
        }
      },
      "Audio Index" => {
        // See AudioConverterParams::input_index.
        if let StateValue::Int(v) = value {
          let mut lock = this.params.lock().unwrap();
          lock.dirty = true;
          lock.params.audio.input_index = v as i32;
        }
      },
      "Video Width" => {
        if let StateValue::Int(v) = value {
          let mut lock = this.params.lock().unwrap();
          lock.dirty = true;
          lock.params.image.width = if v > 0 { Some(v as i32) } else { None };
        }
      },
      "Video Height" => {
        if let StateValue::Int(v) = value {
          let mut lock = this.params.lock().unwrap();
          lock.dirty = true;
          lock.params.image.height = if v > 0 { Some(v as i32) } else { None };
        }
      },
      "MPEG4 Quality" => {
        if let StateValue::Int(v) = value {
          let mut lock = this.params.lock().unwrap();
          lock.dirty = true;
          lock.params.image.mpeg4_quality = if v > 0 { Some(v as i32) } else { None };
        }
      },
      "VP9 Quality" => {
        // VP9 CRF, 0-63; negative uses the default (24).
        if let StateValue::Int(v) = value {
          let mut lock = this.params.lock().unwrap();
          lock.dirty = true;
          lock.params.image.vp9_quality = if v >= 0 { Some(v as i32) } else { None };
        }
      },
      "H264 Quality" => {
        // H264 QP, 1-51; 0 uses the default (18).
        if let StateValue::Int(v) = value {
          let mut lock = this.params.lock().unwrap();
          lock.dirty = true;
          lock.params.image.h264_quality = if v > 0 { Some(v as i32) } else { None };
        }
      },
      "Complete Frames" => {
        if let StateValue::Bool(v) = value {
          let mut lock = this.params.lock().unwrap();
          lock.dirty = true;
          lock.params.image.complete_frames = v;
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
          let notify = borrow.notify.clone();
          let arrivals = borrow.arrivals.clone();
          let api = borrow.api.thread_safe();
          borrow.data_connection = Some(node.subscribe_multithreaded(move |node| {
            let arrived = api.time();
            {
              let mut params = params.lock().unwrap();
              if params.dirty {
                converter.reconfigure(params.params);
                params.dirty = false;
              }
            }

            let data = node.data();
            if data.image().is_some() {
              record_arrival(&arrivals.image, data.time(), arrived);
            }
            if data.analog().is_some() {
              record_arrival(&arrivals.analog, data.time(), arrived);
            }
            converter.push(&data);
            notify.notify_one();
          }));
        }));
        rc.borrow_mut().source_connection = temp;
      }
      _ => {}
    }
  }

  async fn converter_task(
    converter: Arc<MediaConverter>,
    signaler: Arc<OffMainSignaler>,
    notify: Arc<Notify>,
    dropping: Arc<AtomicBool>,
    api: ThalamusAPIThreadSafe,
    arrivals: Arrivals,
    viewer_sink: ImageSink,
  ) {
    let mut last_layout: Option<ChannelLayout> = None;
    let mut emit = |output: &dyn NodeData| {
      // Outputs keep their input's time, which is the key. Each output is
      // either an image or audio.
      let arrivals = if output.image().is_some() { &arrivals.image } else { &arrivals.analog };
      let latency_ms = take_latency_ms(arrivals, output.time(), api.time());
      let mut stats = WithStats::new(output, latency_ms);
      // Format, sample rate, Audio Index and image vs audio outputs all show
      // up as a different layout.
      let layout = channel_layout(&stats);
      stats.channels_changed = last_layout.as_ref() != Some(&layout);
      last_layout = Some(layout);
      let _ = signaler.ready(&stats);
      if let Some(image) = output.image() {
        // Encoded (MPEG4, H264, VP9) output is dropped by the viewer.
        viewer_sink.update(image);
      }
    };
    loop {
      if dropping.load(Ordering::SeqCst) {
        return;
      }
      loop {
        let output = {
          let _trace = api.trace_event(c"MediaConverter::pull");
          converter.pull()
        };
        let Some(output) = output else {
          break;
        };
        emit(&*output);
      }
      notify.notified().await;
    }
  }
}

impl Node for MediaConverterNode {
  fn new(api: ThalamusAPI, node_token: NodeToken, state: State, token: MainThreadToken) -> Rc<RefCell<Self>> {
    let signaler = OffMainSignaler::new(api, node_token);
    signaler.unblock();
    let params = ParamsHolder {
      params: MediaConverterParams {
        image: ConverterParams {
          format: None,
          width: None,
          height: None,
          mpeg4_quality: None,
          complete_frames: false,
          h264_quality: None,
          vp9_quality: None,
        },
        audio: AudioConverterParams::default(),
      },
      dirty: true,
    };
    let result = Rc::new(RefCell::new(MediaConverterNode {
      params: Arc::new(Mutex::new(params.clone())),
      converter: Arc::new(MediaConverter::new(api.thread_safe(), params.params)),
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
      arrivals: Arrivals::default(),
    }));

    let change_ref = Rc::downgrade(&result);
    let state_callback =
      move |s, a, k, v| {
        if let Some(lock) = change_ref.upgrade() {
          MediaConverterNode::on_state(lock, s, a, k, v);
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
      api.tokio().as_ref().unwrap().spawn(MediaConverterNode::converter_task(
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
