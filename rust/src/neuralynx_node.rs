//! NEURALYNX: streams continuously sampled channels and events from a
//! Neuralynx Cheetah server over NetCom.
//!
//! NetCom is loaded at runtime (see netcom.rs); when it isn't installed
//! prepare() fails and Thalamus leaves the node type out.
//!
//! While Running it streams every CSC and event object on the server.
//! Each NetCom CSC callback becomes one analog message carrying every
//! streamed channel, with samples only in the channel the callback was for.
//! Samples are Cheetah's raw i16 AD counts, scaled to microvolts with each
//! channel's ADBitVolts. Events become text messages holding a JSON object.

use std::{
  cell::RefCell,
  collections::HashMap,
  rc::{Rc, Weak},
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
  },
  time::Duration,
};

use crate::api::{
  self, AnalogData, DialogType, MainThreadOnly, MainThreadToken, Node, NodeConsts, NodeData, NodeToken,
  OffMainSignaler, OnDrop, PredropToken, State, StateAction, StateValue, TextData,
  THALAMUS_MODALITY_ANALOG, THALAMUS_MODALITY_TEXT, ThalamusAPI, ThalamusAPIThreadSafe,
};
use crate::netcom::{self, CSC_TYPE, Client, CscRecord, EVENT_TYPE, EventRecord, Handler};

const APPLICATION_NAME: &str = "Thalamus";

struct Settings {
  server: String,
}

impl Settings {
  fn read(state: &State) -> Settings {
    let string = |key: &str| match state.get(api::StateKey::String(key.to_string())) {
      Some(StateValue::String(s)) => s,
      _ => String::new(),
    };
    let server = string("Server").trim().to_string();
    let server = if server.is_empty() { "localhost".to_string() } else { server };
    Settings { server }
  }
}

/// The names of the objects of type `kind`, in server order.
fn objects_of_type<'a>(objects: &'a [(String, String)], kind: &str) -> Vec<&'a String> {
  objects.iter().filter(|(_, t)| t == kind).map(|(n, _)| n).collect()
}

/// The first positive number in a -GetADBitVolts reply.
fn parse_bit_volts(replies: &[String]) -> Option<f64> {
  replies
    .iter()
    .flat_map(|r| r.split_whitespace())
    .filter_map(|t| t.parse::<f64>().ok())
    .find(|v| v.is_finite() && *v > 0.0)
}

/// Quotes an object name for a Cheetah command if it contains whitespace.
fn command_argument(name: &str) -> String {
  if name.chars().any(char::is_whitespace) { format!("\"{name}\"") } else { name.to_string() }
}

fn rate_to_interval(rate: u32) -> Duration {
  if rate == 0 {
    Duration::ZERO
  } else {
    Duration::from_nanos((1_000_000_000 + u64::from(rate) / 2) / u64::from(rate))
  }
}

/// The streamed channels, fixed once the streams are opened except for
/// sample intervals, which come from the records.
#[derive(Default)]
struct Channels {
  names: Vec<String>,
  index: HashMap<String, usize>,
  /// Microvolts per AD count.
  scales: Vec<f64>,
  intervals: Vec<Duration>,
  /// Set until a message reporting a channel change has been delivered.
  changed: bool,
}

impl Channels {
  fn new(names: Vec<String>, scales: Vec<f64>) -> Channels {
    let index = names.iter().enumerate().map(|(i, n)| (n.clone(), i)).collect();
    let intervals = vec![Duration::ZERO; names.len()];
    Channels { names, index, scales, intervals, changed: true }
  }
}

/// One CSC callback's samples.
struct CscMessage<'a> {
  time: Duration,
  channel: usize,
  samples: Vec<i16>,
  channels: &'a Channels,
}

impl NodeData for CscMessage<'_> {
  fn time(&self) -> Duration {
    self.time
  }

  fn analog(&self) -> Option<&dyn AnalogData> {
    Some(self)
  }
}

impl AnalogData for CscMessage<'_> {
  fn short_data(&self, channel: i32) -> &[i16] {
    if usize::try_from(channel) == Ok(self.channel) { &self.samples } else { &[] }
  }

  fn num_channels(&self) -> i32 {
    self.channels.names.len() as i32
  }

  fn sample_interval(&self, channel: i32) -> Duration {
    usize::try_from(channel)
      .ok()
      .and_then(|i| self.channels.intervals.get(i).copied())
      .unwrap_or(Duration::ZERO)
  }

  fn name(&self, channel: i32) -> &str {
    usize::try_from(channel)
      .ok()
      .and_then(|i| self.channels.names.get(i))
      .map_or("", String::as_str)
  }

  fn is_short_data(&self) -> bool {
    true
  }

  fn is_transformed(&self) -> bool {
    true
  }

  fn scale(&self, channel: i32) -> f64 {
    usize::try_from(channel)
      .ok()
      .and_then(|i| self.channels.scales.get(i).copied())
      .unwrap_or(1.0)
  }

  fn channels_changed(&self) -> bool {
    self.channels.changed
  }
}

/// One event record.
struct EventMessage {
  time: Duration,
  text: String,
}

impl NodeData for EventMessage {
  fn time(&self) -> Duration {
    self.time
  }

  fn text(&self) -> Option<&dyn TextData> {
    Some(self)
  }
}

impl TextData for EventMessage {
  fn text(&self) -> &str {
    &self.text
  }
}

fn event_json(object: &str, record: &EventRecord) -> String {
  serde_json::json!({
    "object": object,
    "string": record.string,
    "ttl": record.ttl,
    "event_id": record.event_id,
    "timestamp_us": record.timestamp_us,
  })
  .to_string()
}

/// Shared by the node, the connecting thread and NetCom's callbacks for one
/// Running session.
struct Session {
  api: ThalamusAPIThreadSafe,
  signaler: Arc<OffMainSignaler>,
  stopped: AtomicBool,
  channels: Mutex<Channels>,
  /// Owned here once connected, so stopping can disconnect it.
  client: Mutex<Option<Client>>,
  /// The node's state, taken by whichever failure first sets Running to
  /// false; dropped on the main thread when the session stops otherwise.
  failure_state: Mutex<Option<MainThreadOnly<State>>>,
}

impl Session {
  fn stopped(&self) -> bool {
    self.stopped.load(Ordering::SeqCst)
  }

  /// Sets Running to false, showing `error` if there is one.
  fn fail(&self, error: Option<String>) {
    if let Some(e) = &error {
      println!("NEURALYNX: {e}");
    }
    if self.stopped() {
      return;
    }
    let Some(state) = self.failure_state.lock().unwrap().take() else {
      return;
    };
    let api = self.api;
    api.post_to_main(move |token| {
      let state = state.take(token);
      state.set(api::StateKey::String("Running".to_string()), StateValue::Bool(false));
      if let Some(e) = error {
        api.thread_unsafe(token).show_dialog("Neuralynx", &e, DialogType::Error);
      }
    });
  }

  /// Runs on its own thread: ConnectToServer blocks.
  fn connect(self: Arc<Self>, settings: Settings) {
    let netcom = match netcom::load() {
      Ok(netcom) => netcom,
      Err(e) => return self.fail(Some(e)),
    };
    let client = match Client::new(netcom, self.clone()) {
      Ok(client) => client,
      Err(e) => return self.fail(Some(e)),
    };
    println!("NEURALYNX: NetCom {} from {}", client.version(), netcom.path());
    client.set_application_name(APPLICATION_NAME);
    if !client.connect(&settings.server) {
      return self.fail(Some(format!("Failed to connect to NetCom server {}", settings.server)));
    }
    if self.stopped() {
      return;
    }
    let Some(objects) = client.das_objects() else {
      return self.fail(Some("Failed to get the server's acquisition objects".to_string()));
    };
    println!("NEURALYNX: server objects {objects:?}");

    let names: Vec<String> = objects_of_type(&objects, CSC_TYPE).into_iter().cloned().collect();
    let scales = names
      .iter()
      .map(|name| {
        let reply = client.send_command(&format!("-GetADBitVolts {}", command_argument(name)));
        match reply.and_then(|(_, values)| parse_bit_volts(&values)) {
          Some(bit_volts) => bit_volts * 1e6,
          None => {
            println!("NEURALYNX: couldn't get ADBitVolts for {name}, leaving it unscaled");
            1.0
          }
        }
      })
      .collect();
    *self.channels.lock().unwrap() = Channels::new(names.clone(), scales);

    let events = objects_of_type(&objects, EVENT_TYPE);
    if names.is_empty() && events.is_empty() {
      return self.fail(Some("Nothing to stream: the server has no CSC or event objects".to_string()));
    }
    for name in names.iter().chain(events) {
      if self.stopped() {
        return;
      }
      if !client.open_stream(name) {
        println!("NEURALYNX: failed to open stream {name}");
      }
    }

    let mut slot = self.client.lock().unwrap();
    if !self.stopped() {
      *slot = Some(client);
    }
    // Otherwise the client disconnects as it drops here.
  }

  /// Stops delivering data and disconnects off the main thread.
  fn stop(&self) {
    self.stopped.store(true, Ordering::SeqCst);
    if let Some(client) = self.client.lock().unwrap().take() {
      std::thread::spawn(move || drop(client));
    }
  }
}

impl Handler for Session {
  fn csc(&self, object: &str, records: &[CscRecord]) {
    if self.stopped() {
      return;
    }
    let mut channels = self.channels.lock().unwrap();
    let Some(&channel) = channels.index.get(object) else {
      return;
    };
    if let Some(record) = records.first() {
      let interval = rate_to_interval(record.sample_rate);
      if channels.intervals[channel] != interval {
        channels.intervals[channel] = interval;
        channels.changed = true;
      }
    }
    let samples = records.iter().flat_map(|r| r.samples.iter().copied()).collect();
    let message = CscMessage {
      time: self.api.time(),
      channel,
      samples,
      channels: &channels,
    };
    let delivered = matches!(self.signaler.ready(&message), Ok(true));
    if delivered {
      channels.changed = false;
    }
  }

  fn event(&self, object: &str, records: &[EventRecord]) {
    for record in records {
      if self.stopped() {
        return;
      }
      let message = EventMessage {
        time: self.api.time(),
        text: event_json(object, record),
      };
      let _ = self.signaler.ready(&message);
    }
  }

  fn connection_lost(&self) {
    self.fail(Some("Lost connection to the NetCom server".to_string()));
  }

  fn message(&self, message_type: i32, message: &str) {
    println!("NEURALYNX: NetCom message {message_type}: {message}");
  }
}

pub struct NeuralynxNode {
  api: ThalamusAPI,
  state: State,
  main_thread_token: MainThreadToken,
  signaler: Arc<OffMainSignaler>,
  _state_connection: OnDrop,
  session: Option<Arc<Session>>,
}

impl NeuralynxNode {
  fn stop(&mut self) {
    self.signaler.block();
    if let Some(session) = self.session.take() {
      session.stop();
      // Dropped here, on the main thread, unless a failure already took it.
      session.failure_state.lock().unwrap().take();
    }
  }

  fn start(&mut self) {
    let settings = Settings::read(&self.state);
    let session = Arc::new(Session {
      api: self.api.thread_safe(),
      signaler: self.signaler.clone(),
      stopped: AtomicBool::new(false),
      channels: Mutex::new(Channels::default()),
      client: Mutex::new(None),
      failure_state: Mutex::new(Some(MainThreadOnly::new(
        self.state.clone(),
        self.api,
        self.main_thread_token,
      ))),
    });
    self.signaler.unblock();
    let connecting = session.clone();
    std::thread::spawn(move || connecting.connect(settings));
    self.session = Some(session);
  }

  fn on_state(&mut self, source: State, _action: StateAction, key: StateValue, value: StateValue) {
    if source != self.state {
      return;
    }
    let StateValue::String(key) = key else {
      return;
    };
    if key == "Running" {
      self.stop();
      if value == StateValue::Bool(true) {
        self.start();
      }
    }
  }
}

impl NodeConsts for NeuralynxNode {
  const MODALITIES: u32 = THALAMUS_MODALITY_ANALOG | THALAMUS_MODALITY_TEXT;
  const SIGNALS_OFFMAIN: bool = true;
}

impl Node for NeuralynxNode {
  fn new(api: ThalamusAPI, node_token: NodeToken, state: State, main_thread_token: MainThreadToken) -> Rc<RefCell<Self>> {
    let result = Rc::new_cyclic(|weak: &Weak<RefCell<Self>>| {
      let weak = weak.clone();
      let _state_connection = state.connect(move |source, action, key, value| {
        if let Some(strong) = weak.upgrade() {
          strong.borrow_mut().on_state(source, action, key, value);
        }
      });
      RefCell::new(Self {
        api,
        state: state.clone(),
        main_thread_token,
        signaler: OffMainSignaler::new(api, node_token.clone()),
        _state_connection,
        session: None,
      })
    });
    state.recap();
    result
  }

  fn prepare() -> bool {
    match netcom::load() {
      Ok(netcom) => {
        println!("NEURALYNX: loaded {}", netcom.path());
        true
      }
      Err(e) => {
        println!("NEURALYNX: {e}. Neuralynx features disabled");
        false
      }
    }
  }

  fn predrop(&self, token: PredropToken) {
    if let Some(session) = &self.session {
      session.stopped.store(true, Ordering::SeqCst);
    }
    self.signaler.predrop(token);
  }
}

impl Drop for NeuralynxNode {
  fn drop(&mut self) {
    self.stop();
    self.state.set(api::StateKey::String("Running".to_string()), StateValue::Bool(false));
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn objects() -> Vec<(String, String)> {
    [("CSC1", CSC_TYPE), ("Events", EVENT_TYPE), ("CSC 2", CSC_TYPE), ("TT1", "TTScAcqEnt")]
      .iter()
      .map(|(n, t)| (n.to_string(), t.to_string()))
      .collect()
  }

  #[test]
  fn objects_are_selected_by_type_in_server_order() {
    let objects = objects();
    assert_eq!(objects_of_type(&objects, CSC_TYPE), vec!["CSC1", "CSC 2"]);
    assert_eq!(objects_of_type(&objects, EVENT_TYPE), vec!["Events"]);
  }

  #[test]
  fn bit_volts_is_the_first_positive_number() {
    assert_eq!(parse_bit_volts(&["0".to_string(), "3.05e-8".to_string()]), Some(3.05e-8));
    assert_eq!(parse_bit_volts(&["0 0.000000061".to_string()]), Some(0.000000061));
    assert_eq!(parse_bit_volts(&["error".to_string()]), None);
  }

  #[test]
  fn command_arguments_quote_names_with_spaces() {
    assert_eq!(command_argument("CSC1"), "CSC1");
    assert_eq!(command_argument("CSC 1"), "\"CSC 1\"");
  }

  #[test]
  fn rates_become_whole_nanosecond_intervals() {
    assert_eq!(rate_to_interval(32000), Duration::from_nanos(31250));
    assert_eq!(rate_to_interval(30000), Duration::from_nanos(33333));
    assert_eq!(rate_to_interval(0), Duration::ZERO);
  }

  #[test]
  fn csc_messages_only_fill_their_channel() {
    let mut channels = Channels::new(vec!["CSC1".to_string(), "CSC2".to_string()], vec![0.5, 0.25]);
    channels.intervals[1] = Duration::from_nanos(31250);
    let message = CscMessage {
      time: Duration::from_secs(1),
      channel: 1,
      samples: vec![1, -2, 3],
      channels: &channels,
    };
    assert_eq!(message.num_channels(), 2);
    assert_eq!(message.name(1), "CSC2");
    assert_eq!(message.name(2), "");
    assert_eq!(message.short_data(0), &[] as &[i16]);
    assert_eq!(message.short_data(1), &[1, -2, 3]);
    assert_eq!(message.short_data(-1), &[] as &[i16]);
    assert_eq!(message.scale(0), 0.5);
    assert_eq!(message.sample_interval(1), Duration::from_nanos(31250));
    assert!(message.channels_changed());
    assert!(message.analog().is_some() && message.text().is_none());
  }

  #[test]
  fn events_are_json() {
    let record = EventRecord {
      timestamp_us: 123,
      event_id: 11,
      ttl: 4,
      string: "TTL Input on AcqSystem1_0 board 0 port 0 value (0x0004).".to_string(),
    };
    let json: serde_json::Value = serde_json::from_str(&event_json("Events", &record)).unwrap();
    assert_eq!(json["object"], "Events");
    assert_eq!(json["ttl"], 4);
    assert_eq!(json["timestamp_us"], 123);
    assert_eq!(json["string"], record.string);
  }
}
