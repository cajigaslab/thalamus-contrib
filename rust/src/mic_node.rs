use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{I24, SampleFormat, SizedSample, U24};
use std::{
  cell::RefCell,
  rc::{Rc, Weak},
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
  },
  time::Duration,
};

use crate::api::{
  self, AnalogData, Json, MainThreadOnly, MainThreadToken, Node, NodeConsts, NodeData, NodeToken,
  OffMainSignaler, OnDrop, Request, State, StateAction, StateValue, THALAMUS_MODALITY_ANALOG,
  ThalamusAPI, ThalamusAPIThreadSafe,
};

/// Sample rates offered for devices that report a range of supported rates.
const STANDARD_SAMPLE_RATES: [u32; 9] = [8000, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000];

/// Buffers queued between the audio callback and the node's thread; when the
/// node's thread falls this far behind, new buffers are dropped rather than
/// blocking the audio thread.
const QUEUE_DEPTH: usize = 64;

/// Captured samples, one Vec per channel, in the Thalamus analog type that
/// holds the stream's sample format without loss. Formats Thalamus has no
/// type for are widened; their values are unchanged (unsigned formats keep
/// their offset, e.g. u8 silence is still 128).
#[derive(Debug, PartialEq)]
enum Samples {
  /// i8, u8, i16
  Short(Vec<Vec<i16>>),
  /// u16, I24, U24, i32
  Int(Vec<Vec<i32>>),
  /// u32, u64
  ULong(Vec<Vec<u64>>),
  /// f32 (converted), f64
  Double(Vec<Vec<f64>>),
}

impl Samples {
  fn channel_count(&self) -> usize {
    match self {
      Samples::Short(c) => c.len(),
      Samples::Int(c) => c.len(),
      Samples::ULong(c) => c.len(),
      Samples::Double(c) => c.len(),
    }
  }
}

/// Splits interleaved frames into one Vec per channel, converting each
/// sample. A trailing partial frame is dropped.
fn deinterleave<T: Copy, O>(data: &[T], channel_count: usize, convert: impl Fn(T) -> O) -> Vec<Vec<O>> {
  let frames = data.len() / channel_count;
  let mut channels: Vec<Vec<O>> = (0..channel_count).map(|_| Vec::with_capacity(frames)).collect();
  for frame in data.chunks_exact(channel_count) {
    for (channel, &sample) in channels.iter_mut().zip(frame) {
      channel.push(convert(sample));
    }
  }
  channels
}

/// A cpal sample type the node can emit.
trait CaptureSample: SizedSample {
  fn samples(data: &[Self], channel_count: usize) -> Samples;
}

macro_rules! capture_sample {
  ($t:ty, $variant:ident, $convert:expr) => {
    impl CaptureSample for $t {
      fn samples(data: &[Self], channel_count: usize) -> Samples {
        Samples::$variant(deinterleave(data, channel_count, $convert))
      }
    }
  };
}

capture_sample!(i8, Short, i16::from);
capture_sample!(u8, Short, i16::from);
capture_sample!(i16, Short, |s| s);
capture_sample!(u16, Int, i32::from);
capture_sample!(I24, Int, |s: I24| s.inner());
capture_sample!(U24, Int, |s: U24| s.inner());
capture_sample!(i32, Int, |s| s);
capture_sample!(u32, ULong, u64::from);
capture_sample!(u64, ULong, |s| s);
capture_sample!(f32, Double, f64::from);
capture_sample!(f64, Double, |s| s);

/// Whether the node can emit `format`; i64 has no lossless Thalamus type.
fn is_emittable(format: SampleFormat) -> bool {
  matches!(
    format,
    SampleFormat::I8
      | SampleFormat::U8
      | SampleFormat::I16
      | SampleFormat::U16
      | SampleFormat::I24
      | SampleFormat::U24
      | SampleFormat::I32
      | SampleFormat::U32
      | SampleFormat::U64
      | SampleFormat::F32
      | SampleFormat::F64
  )
}

/// One buffer of captured audio.
struct Chunk {
  samples: Samples,
  names: Arc<Vec<String>>,
  sample_interval: Duration,
  time: Duration,
}

impl NodeData for Chunk {
  fn analog(&self) -> Option<&dyn AnalogData> {
    Some(self)
  }

  fn image(&self) -> Option<&dyn api::ImageData> {
    None
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

/// `channels[channel]`, or empty for an out-of-range channel.
fn channel_slice<T>(channels: &[Vec<T>], channel: i32) -> &[T] {
  usize::try_from(channel)
    .ok()
    .and_then(|i| channels.get(i))
    .map_or(&[], |c| c.as_slice())
}

impl AnalogData for Chunk {
  fn data(&self, channel: i32) -> &[f64] {
    match &self.samples {
      Samples::Double(c) => channel_slice(c, channel),
      _ => &[],
    }
  }

  fn short_data(&self, channel: i32) -> &[i16] {
    match &self.samples {
      Samples::Short(c) => channel_slice(c, channel),
      _ => &[],
    }
  }

  fn int_data(&self, channel: i32) -> &[i32] {
    match &self.samples {
      Samples::Int(c) => channel_slice(c, channel),
      _ => &[],
    }
  }

  fn ulong_data(&self, channel: i32) -> &[u64] {
    match &self.samples {
      Samples::ULong(c) => channel_slice(c, channel),
      _ => &[],
    }
  }

  fn is_short_data(&self) -> bool {
    matches!(self.samples, Samples::Short(_))
  }

  fn is_int_data(&self) -> bool {
    matches!(self.samples, Samples::Int(_))
  }

  fn is_ulong_data(&self) -> bool {
    matches!(self.samples, Samples::ULong(_))
  }

  fn num_channels(&self) -> i32 {
    self.samples.channel_count() as i32
  }

  fn sample_interval(&self, _channel: i32) -> Duration {
    self.sample_interval
  }

  fn name(&self, channel: i32) -> &str {
    usize::try_from(channel)
      .ok()
      .and_then(|i| self.names.get(i))
      .map_or("", |n| n.as_str())
  }
}

fn channel_names(count: u16) -> Arc<Vec<String>> {
  Arc::new((0..count).map(|i| format!("Channel {i}")).collect())
}

fn find_device(id: Option<&str>) -> Option<cpal::Device> {
  let host = cpal::default_host();
  match id {
    Some(id) => {
      let id = id.parse::<cpal::DeviceId>().ok()?;
      host.device_by_id(&id)
    }
    None => host.default_input_device(),
  }
}

fn device_json(device: &cpal::Device) -> Option<serde_json::Value> {
  let id = device.id().ok()?;
  let name = device
    .description()
    .map(|d| d.name().to_string())
    .unwrap_or_else(|_| id.to_string());
  Some(serde_json::json!({ "id": id.to_string(), "name": name }))
}

fn get_devices() -> Vec<serde_json::Value> {
  match cpal::default_host().input_devices() {
    Ok(devices) => devices.filter_map(|d| device_json(&d)).collect(),
    Err(e) => {
      println!("Audio device query failed: {e}");
      Vec::new()
    }
  }
}

/// The object stored in the node's "Format" config and returned by get_formats.
fn format_json(channels: u16, sample_rate: u32) -> serde_json::Value {
  serde_json::json!({ "channels": channels, "sample_rate": sample_rate })
}

/// (channels, sample rate) pairs the device can capture, most channels and
/// highest rate first.
fn supported_formats(device: &cpal::Device) -> Vec<(u16, u32)> {
  let mut formats = Vec::new();
  if let Ok(configs) = device.supported_input_configs() {
    for config in configs {
      let (min, max) = (config.min_sample_rate(), config.max_sample_rate());
      if min == max {
        formats.push((config.channels(), min));
        continue;
      }
      for rate in STANDARD_SAMPLE_RATES.into_iter().filter(|r| (min..=max).contains(r)) {
        formats.push((config.channels(), rate));
      }
    }
  }
  formats.sort_by_key(|&(channels, rate)| (std::cmp::Reverse(channels), std::cmp::Reverse(rate)));
  formats.dedup();
  formats
}

/// The stream config to capture with: the requested channels and rate if the
/// device supports them (in the device's default sample format when it offers
/// that), otherwise its default input config.
fn choose_config(
  device: &cpal::Device,
  requested: Option<(u16, u32)>,
) -> Result<cpal::SupportedStreamConfig, String> {
  let default = device.default_input_config().map_err(|e| e.to_string())?;
  if let Some((channels, rate)) = requested {
    let best = device
      .supported_input_configs()
      .map_err(|e| e.to_string())?
      .filter(|c| {
        c.channels() == channels
          && (c.min_sample_rate()..=c.max_sample_rate()).contains(&rate)
          && is_emittable(c.sample_format())
      })
      .min_by_key(|c| c.sample_format() != default.sample_format());
    if let Some(config) = best {
      return Ok(config.with_sample_rate(rate));
    }
    println!("MIC: {channels} channels at {rate} Hz isn't supported, using the device default");
  }
  Ok(default)
}

/// Snapshot of the node's config, read on the main thread.
struct MicSettings {
  /// None means the default input device.
  device_id: Option<String>,
  format: Option<(u16, u32)>,
}

impl MicSettings {
  fn read(state: &State) -> MicSettings {
    // The widget stores one of the objects returned by get_devices; an empty
    // id means the default device.
    let device_id = match state.get(api::StateKey::String("Device".to_string())) {
      Some(StateValue::Dict(device)) => match device.get(api::StateKey::String("id".to_string())) {
        Some(StateValue::String(id)) if !id.is_empty() => Some(id),
        _ => None,
      },
      _ => None,
    };
    // The widget stores one of the objects returned by get_formats.
    let format = match state.get(api::StateKey::String("Format".to_string())) {
      Some(StateValue::Dict(format)) => {
        let int = |key: &str| match format.get(api::StateKey::String(key.to_string())) {
          Some(StateValue::Int(v)) => Some(v),
          _ => None,
        };
        let channels = int("channels").and_then(|c| u16::try_from(c).ok());
        let rate = int("sample_rate").and_then(|r| u32::try_from(r).ok());
        channels.zip(rate)
      }
      _ => None,
    };
    MicSettings { device_id, format }
  }
}

pub struct MicNode {
  api: ThalamusAPI,
  _state_connection: OnDrop,
  node_token: NodeToken,
  main_thread_token: MainThreadToken,
  state: State,
  signaler: Arc<OffMainSignaler>,
  mic_thread: Option<std::thread::JoinHandle<()>>,
  /// Tells the mic thread to stop even if no audio is arriving.
  stop: Arc<AtomicBool>,
}

/// Opens an input stream whose callback converts each buffer into a Chunk
/// (timestamped by `clock`) and queues it on `sender`. Buffers are dropped
/// when the queue is full; the callback never blocks the audio thread.
fn build_stream<T: CaptureSample>(
  device: &cpal::Device,
  config: &cpal::StreamConfig,
  names: Arc<Vec<String>>,
  clock: impl Fn() -> Duration + Send + 'static,
  sender: mpsc::SyncSender<Chunk>,
  failed: Arc<AtomicBool>,
) -> Result<cpal::Stream, cpal::Error> {
  let channel_count = usize::from(config.channels);
  let sample_interval = Duration::from_secs_f64(1.0 / f64::from(config.sample_rate));
  device.build_input_stream::<T, _, _>(
    config.clone(),
    move |data: &[T], _| {
      let _ = sender.try_send(Chunk {
        samples: T::samples(data, channel_count),
        names: names.clone(),
        sample_interval,
        time: clock(),
      });
    },
    move |e| {
      println!("MIC stream error: {e}");
      failed.store(true, Ordering::SeqCst);
    },
    None,
  )
}

/// Opens a stream in `config`'s sample format; see build_stream.
fn open_stream(
  device: &cpal::Device,
  config: &cpal::SupportedStreamConfig,
  names: Arc<Vec<String>>,
  clock: impl Fn() -> Duration + Send + 'static,
  sender: mpsc::SyncSender<Chunk>,
  failed: Arc<AtomicBool>,
) -> Result<cpal::Stream, String> {
  let stream_config = config.config();
  macro_rules! build {
    ($t:ty) => {
      build_stream::<$t>(device, &stream_config, names, clock, sender, failed)
    };
  }
  let stream = match config.sample_format() {
    SampleFormat::I8 => build!(i8),
    SampleFormat::U8 => build!(u8),
    SampleFormat::I16 => build!(i16),
    SampleFormat::U16 => build!(u16),
    SampleFormat::I24 => build!(I24),
    SampleFormat::U24 => build!(U24),
    SampleFormat::I32 => build!(i32),
    SampleFormat::U32 => build!(u32),
    SampleFormat::U64 => build!(u64),
    SampleFormat::F32 => build!(f32),
    SampleFormat::F64 => build!(f64),
    other => return Err(format!("unsupported sample format {other:?}")),
  };
  stream.map_err(|e| e.to_string())
}

impl MicNode {
  fn stop_mic(&mut self) {
    self.stop.store(true, Ordering::SeqCst);
    self.signaler.block();
    self.mic_thread.take().map(|h| h.join());
  }

  /// Runs on the mic thread: owns the cpal stream (which isn't Send on every
  /// platform) and forwards its buffers until stopped or the stream fails.
  fn mic(
    signaler: Arc<OffMainSignaler>,
    device_id: Option<String>,
    config: cpal::SupportedStreamConfig,
    names: Arc<Vec<String>>,
    api: ThalamusAPIThreadSafe,
    stop: Arc<AtomicBool>,
  ) {
    let Some(device) = find_device(device_id.as_deref()) else {
      println!("MIC: audio device not found");
      return;
    };
    let (sender, receiver) = mpsc::sync_channel(QUEUE_DEPTH);
    let failed = Arc::new(AtomicBool::new(false));
    let stream = match open_stream(&device, &config, names, move || api.time(), sender, failed.clone()) {
      Ok(stream) => stream,
      Err(e) => {
        println!("MIC: failed to open stream: {e}");
        return;
      }
    };
    if let Err(e) = stream.play() {
      println!("MIC: failed to start stream: {e}");
      return;
    }

    while !stop.load(Ordering::SeqCst) && !failed.load(Ordering::SeqCst) {
      match receiver.recv_timeout(Duration::from_millis(100)) {
        Ok(chunk) => match signaler.ready(&chunk) {
          Ok(true) => {}
          _ => break,
        },
        Err(mpsc::RecvTimeoutError::Timeout) => {}
        Err(mpsc::RecvTimeoutError::Disconnected) => break,
      }
    }
    println!("mic end");
  }

  fn start_mic(&mut self) {
    let settings = MicSettings::read(&self.state);
    let Some(device) = find_device(settings.device_id.as_deref()) else {
      println!("MIC: audio device not found");
      self.set_running_false();
      return;
    };
    let config = match choose_config(&device, settings.format) {
      Ok(config) => config,
      Err(e) => {
        println!("MIC: no usable input config: {e}");
        self.set_running_false();
        return;
      }
    };
    // Resolved here rather than on the mic thread so downstream nodes can be
    // told about the channels before the first samples arrive.
    let names = channel_names(config.channels());
    let _ = self.api.channels_changed(&self.node_token);

    self.stop.store(false, Ordering::SeqCst);
    let api = self.api.thread_safe();
    let signaler = self.signaler.clone();
    signaler.unblock();
    let wrapped_state = MainThreadOnly::new(self.state.clone(), self.main_thread_token);
    let stop = self.stop.clone();
    let device_id = settings.device_id;
    self.mic_thread = Some(std::thread::spawn(move || {
      MicNode::mic(signaler, device_id, config, names, api, stop);
      api.post_to_main(|main_thread_token| {
        let state = wrapped_state.take(main_thread_token);
        state.set(
          api::StateKey::String("Running".to_string()),
          api::StateValue::Bool(false),
        );
      });
    }));
  }

  /// Posted rather than set directly: this runs inside the state callback.
  fn set_running_false(&self) {
    let state = MainThreadOnly::new(self.state.clone(), self.main_thread_token);
    self.api.thread_safe().post_to_main(move |main_thread_token| {
      state.take(main_thread_token).set(
        api::StateKey::String("Running".to_string()),
        api::StateValue::Bool(false),
      );
    });
  }

  fn on_state(&mut self, _source: State, _action: StateAction, key: StateValue, value: StateValue) {
    let StateValue::String(key_str) = key else {
      return;
    };
    if key_str == "Running" {
      self.stop_mic();
      if value == StateValue::Bool(true) {
        self.start_mic();
      }
    }
  }

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
      Some("get_devices") => serde_json::to_string(&get_devices()).unwrap(),
      Some("get_formats") => {
        // An empty or missing id means the default device.
        let id = request
          .get("id")
          .and_then(|i| i.as_str())
          .filter(|i| !i.is_empty());
        let formats: Vec<serde_json::Value> = find_device(id)
          .map(|device| {
            supported_formats(&device)
              .into_iter()
              .map(|(channels, rate)| format_json(channels, rate))
              .collect()
          })
          .unwrap_or_default();
        serde_json::to_string(&formats).unwrap()
      }
      _ => "null".to_string(),
    };
    handle.respond(&Json::from_string(api, &response));
  }
}

impl NodeConsts for MicNode {
  const MODALITIES: u32 = THALAMUS_MODALITY_ANALOG;
  const SIGNALS_OFFMAIN: bool = true;
}

impl Node for MicNode {
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
          strong.borrow_mut().on_state(source, action, key, value);
        }
      };
      let _state_connection = state.connect(callback);
      RefCell::new(Self {
        api,
        _state_connection,
        node_token: node_token.clone(),
        main_thread_token,
        state: state.clone(),
        signaler,
        mic_thread: None,
        stop: Arc::new(AtomicBool::new(false)),
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

impl Drop for MicNode {
  fn drop(&mut self) {
    self.stop_mic();
    self.state.set(
      api::StateKey::String("Running".to_string()),
      api::StateValue::Bool(false),
    );
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn deinterleave_splits_channels_and_drops_partial_frames() {
    let data = [1, 2, 3, 4, 5, 6, 7];
    assert_eq!(deinterleave(&data, 2, |s| s), vec![vec![1, 3, 5], vec![2, 4, 6]]);
    assert_eq!(deinterleave(&data, 3, |s| s), vec![vec![1, 4], vec![2, 5], vec![3, 6]]);
    assert_eq!(deinterleave::<i32, i32>(&[], 2, |s| s), vec![Vec::<i32>::new(), vec![]]);
  }

  #[test]
  fn samples_use_the_lossless_thalamus_type() {
    assert_eq!(i16::samples(&[-32768, 32767], 1), Samples::Short(vec![vec![-32768, 32767]]));
    assert_eq!(i8::samples(&[-128, 127], 1), Samples::Short(vec![vec![-128, 127]]));
    // Unsigned values are widened unchanged, offset included.
    assert_eq!(u8::samples(&[0, 128, 255], 1), Samples::Short(vec![vec![0, 128, 255]]));
    assert_eq!(u16::samples(&[0, 65535], 1), Samples::Int(vec![vec![0, 65535]]));
    assert_eq!(
      I24::samples(&[I24::new(-8_388_608).unwrap(), I24::new(8_388_607).unwrap()], 1),
      Samples::Int(vec![vec![-8_388_608, 8_388_607]])
    );
    assert_eq!(i32::samples(&[i32::MIN, i32::MAX], 1), Samples::Int(vec![vec![i32::MIN, i32::MAX]]));
    assert_eq!(u32::samples(&[u32::MAX], 1), Samples::ULong(vec![vec![u64::from(u32::MAX)]]));
    assert_eq!(u64::samples(&[u64::MAX], 1), Samples::ULong(vec![vec![u64::MAX]]));
    assert_eq!(f64::samples(&[-1.0, 0.25], 1), Samples::Double(vec![vec![-1.0, 0.25]]));
  }

  #[test]
  fn f32_samples_convert_to_f64_exactly() {
    let data = [-1.0f32, 0.1, 0.999_999_94, 1.5];
    let Samples::Double(channels) = f32::samples(&data, 1) else {
      panic!("f32 should become Double");
    };
    let expected: Vec<f64> = data.iter().map(|&s| f64::from(s)).collect();
    assert_eq!(channels, vec![expected]);
    // Out-of-range floats aren't clipped.
    assert_eq!(channels[0][3], 1.5);
  }

  #[test]
  fn i64_is_not_emittable() {
    assert!(!is_emittable(SampleFormat::I64));
    assert!(is_emittable(SampleFormat::F32));
    assert!(is_emittable(SampleFormat::I16));
  }

  fn chunk(samples: Samples) -> Chunk {
    let names = channel_names(samples.channel_count() as u16);
    Chunk {
      samples,
      names,
      sample_interval: Duration::from_micros(20),
      time: Duration::from_secs(1),
    }
  }

  #[test]
  fn chunk_reports_its_sample_type() {
    let short = chunk(Samples::Short(vec![vec![1, 2], vec![3, 4]]));
    assert!(short.is_short_data() && !short.is_int_data() && !short.is_ulong_data());
    assert_eq!(short.short_data(1), &[3, 4]);
    assert_eq!(short.data(0), &[] as &[f64]);

    let int = chunk(Samples::Int(vec![vec![5]]));
    assert!(int.is_int_data() && !int.is_short_data());
    assert_eq!(int.int_data(0), &[5]);

    let ulong = chunk(Samples::ULong(vec![vec![6]]));
    assert!(ulong.is_ulong_data());
    assert_eq!(ulong.ulong_data(0), &[6]);

    let double = chunk(Samples::Double(vec![vec![0.5]]));
    assert!(!double.is_short_data() && !double.is_int_data() && !double.is_ulong_data());
    assert_eq!(double.data(0), &[0.5]);
  }

  #[test]
  fn chunk_channels_and_names() {
    let c = chunk(Samples::Short(vec![vec![1], vec![2]]));
    assert_eq!(c.num_channels(), 2);
    assert_eq!(c.name(0), "Channel 0");
    assert_eq!(c.name(1), "Channel 1");
    assert_eq!(c.name(2), "");
    assert_eq!(c.short_data(2), &[] as &[i16]);
    assert_eq!(c.short_data(-1), &[] as &[i16]);
    assert_eq!(c.sample_interval(0), Duration::from_micros(20));
  }

  /// Captures half a second from the default input device through the same
  /// path the node uses. Needs audio hardware, so it's ignored by default:
  /// cargo test capture_from_default_device -- --ignored --nocapture
  #[test]
  #[ignore]
  fn capture_from_default_device() {
    for device in get_devices() {
      println!("device {device}");
    }
    let device = find_device(None).expect("no default input device");
    println!("formats {:?}", supported_formats(&device));
    let config = choose_config(&device, None).unwrap();
    println!("config {config:?}");

    let (sender, receiver) = mpsc::sync_channel(QUEUE_DEPTH);
    let failed = Arc::new(AtomicBool::new(false));
    let start = std::time::Instant::now();
    let names = channel_names(config.channels());
    let stream = open_stream(&device, &config, names, move || start.elapsed(), sender, failed.clone()).unwrap();
    stream.play().unwrap();

    let mut buffers = 0;
    let mut frames = 0;
    while start.elapsed() < Duration::from_millis(500) {
      if let Ok(chunk) = receiver.recv_timeout(Duration::from_millis(100)) {
        assert_eq!(chunk.num_channels(), i32::from(config.channels()));
        buffers += 1;
        frames += match &chunk.samples {
          Samples::Short(c) => c[0].len(),
          Samples::Int(c) => c[0].len(),
          Samples::ULong(c) => c[0].len(),
          Samples::Double(c) => c[0].len(),
        };
      }
    }
    println!("buffers {buffers} frames {frames}");
    assert!(!failed.load(Ordering::SeqCst), "stream reported an error");
    assert!(buffers > 0, "no audio arrived");
  }
}
