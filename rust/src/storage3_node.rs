//! STORAGE3: records events, log messages and data from source nodes to a
//! list of outputs, in the length prefixed StorageRecord format of
//! Thalamus's STORAGE2 (.tha files), without its file copying, Simple Copy
//! mode or compression.
//!
//! Each output is a URL. file:// URLs and plain paths are local files;
//! http:// and https:// URLs receive the recording as one POST whose body is
//! streamed (chunked) until recording stops, as Thoth's POST /files/PATH
//! expects.
//! Like STORAGE2, the config is also written next to each output, with
//! ".json" appended.
//!
//! Outputs are {"URL": ..., "Main": bool, "Hash URL": ...} objects, exactly
//! one of them Main (the node keeps it that way). A non-empty Hash URL (a
//! path or URL, rendered like the output's name) receives the Adler-32 of
//! the output, as 8 hex digits, once the output finishes: written to a file
//! or POSTed, as Thoth's POST /hashes/PATH expects. Output names are rendered like STORAGE2's:
//! {{rec}} becomes the zero padded recording number, strftime fields the
//! start time, and ".YYYYMMDD.N" is appended, N being the first recording
//! number for which the Main output doesn't exist yet. Every output gets
//! that number.
//!
//! Every output is fed the same buffers, files by their own thread and HTTP
//! outputs by a task on the plugin's tokio runtime (reqwest): one that fails
//! is logged and dropped without affecting the others.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::format::{Item, StrftimeItems};
use chrono::{DateTime, Local};
use futures::Stream;
use reqwest::header::CONTENT_TYPE;
use tokio::runtime::Handle;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

use crate::api::{
  AnalogData, DialogType, Json, MainThreadOnly, MainThreadToken, Node, NodeConsts, NodeData,
  NodeSelector, NodeToken, OnDrop, Request, State, StateAction, StateKey, StateValue,
  THALAMUS_MODALITY_ANALOG, ThalamusAPI, ThalamusAPIThreadSafe, Timer,
};
use crate::build_info;
use crate::deferred;
use crate::storage_record::{self, MetadataValue};

const STATS_INTERVAL: Duration = Duration::from_secs(1);
/// How often the writer thread checks whether recording stopped while it
/// waits for the main thread.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Gives up looking for an unused recording number after this many.
const MAX_REC: u32 = 100_000;
/// Only connecting is limited: an upload lasts as long as the recording.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Where an output goes.
#[derive(Debug, Clone, PartialEq)]
enum Output {
  /// A local file path (template).
  File(String),
  /// An http:// or https:// URL (template).
  Http(String),
}

impl Output {
  fn template(&self) -> &str {
    match self {
      Output::File(template) | Output::Http(template) => template,
    }
  }
}

/// Parses an output URL. Anything without a "scheme://" prefix is a path.
fn parse_output(url: &str) -> Result<Output, String> {
  let url = url.trim();
  if url.is_empty() {
    return Err("empty output".to_string());
  }
  let scheme = url.find("://").map(|end| &url[..end]).filter(|scheme| {
    !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
  });
  let Some(scheme) = scheme else {
    return Ok(Output::File(url.to_string()));
  };
  let rest = &url[scheme.len() + 3..];
  match scheme.to_ascii_lowercase().as_str() {
    "file" => {
      // file:///C:/data/x.tha is the Windows path C:/data/x.tha.
      let bytes = rest.as_bytes();
      let path = if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        &rest[1..]
      } else {
        rest
      };
      if path.is_empty() {
        return Err(format!("{url} has no path"));
      }
      Ok(Output::File(path.to_string()))
    }
    "http" | "https" => Ok(Output::Http(url.to_string())),
    other => Err(format!("{url}: unsupported protocol {other}")),
  }
}

/// Replaces every {{rec}} (spaces allowed inside the braces) with `rec`.
fn replace_rec(template: &str, rec: &str) -> String {
  let mut result = String::new();
  let mut rest = template;
  while let Some(start) = rest.find("{{") {
    let Some(length) = rest[start + 2..].find("}}") else {
      break;
    };
    let inner = &rest[start + 2..start + 2 + length];
    result.push_str(&rest[..start]);
    if inner.trim() == "rec" {
      result.push_str(rec);
    } else {
      result.push_str(&rest[start..start + 4 + length]);
    }
    rest = &rest[start + 4 + length..];
  }
  result.push_str(rest);
  result
}

/// STORAGE2's name for recording `rec` started at `time`: {{rec}} and
/// strftime fields rendered, then ".YYYYMMDD.rec". Templates strftime can't
/// parse (e.g. URLs with percent escapes) keep their % fields as they are.
fn recording_name(template: &str, rec: u32, time: &DateTime<Local>) -> String {
  let with_rec = replace_rec(template, &format!("{rec:03}"));
  let items: Vec<Item> = StrftimeItems::new(&with_rec).collect();
  let mut rendered = String::new();
  if items.iter().any(|item| matches!(item, Item::Error))
    || write!(rendered, "{}", time.format_with_items(items.into_iter())).is_err()
  {
    rendered = with_rec.clone();
  }
  format!("{rendered}.{}.{rec}", time.format("%Y%m%d"))
}

fn http_client() -> Result<reqwest::Client, String> {
  reqwest::Client::builder()
    .connect_timeout(CONNECT_TIMEOUT)
    .build()
    .map_err(|error| format!("couldn't create an HTTP client: {}", error_chain(&error)))
}

/// An error and its sources: reqwest's own message doesn't say why.
fn error_chain(error: &dyn std::error::Error) -> String {
  let mut message = error.to_string();
  let mut source = error.source();
  while let Some(error) = source {
    message.push_str(": ");
    message.push_str(&error.to_string());
    source = error.source();
  }
  message
}

/// Prints `message` and sends it to Thalamus's log, so it's in the log view
/// and recordings too (on a Thalamus with plugin logging).
fn report(api: ThalamusAPIThreadSafe, message: &str) {
  let text = format!("STORAGE3: {message}");
  println!("{text}");
  api.log(&text);
}

/// Whether recording `name` of `output` already exists. A server that
/// can't be asked counts as not having it: the output will fail on its own.
/// Called from the writer thread, which isn't a runtime thread.
fn output_exists(
  api: ThalamusAPIThreadSafe,
  output: &Output,
  name: &str,
  client: &reqwest::Client,
  runtime: &Handle,
) -> bool {
  match output {
    Output::File(_) => Path::new(name).exists(),
    Output::Http(_) => match runtime.block_on(client.head(name).send()) {
      Ok(response) => response.status().is_success(),
      Err(error) => {
        report(api, &format!("couldn't check whether {name} exists: {}", error_chain(&error)));
        false
      }
    },
  }
}

/// The first recording number for which the main output doesn't exist.
fn next_rec(main: &Output, time: &DateTime<Local>, exists: impl Fn(&Output, &str) -> bool) -> u32 {
  (1..MAX_REC).find(|&rec| !exists(main, &recording_name(main.template(), rec, time))).unwrap_or(MAX_REC)
}

/// A buffer of framed records, shared by every output.
type Chunk = Bytes;

/// An output's running Adler-32, if it has a Hash URL.
type Hasher = Arc<Mutex<Option<adler2::Adler32>>>;

fn new_hasher(hash: bool) -> Hasher {
  Arc::new(Mutex::new(hash.then(adler2::Adler32::new)))
}

fn hash_chunk(hasher: &Hasher, chunk: &[u8]) {
  if let Some(adler) = hasher.lock().unwrap().as_mut() {
    adler.write_slice(chunk);
  }
}

fn checksum(hasher: &Hasher) -> Option<u32> {
  hasher.lock().unwrap().as_ref().map(|adler| adler.checksum())
}

/// The body of an HTTP output: the chunks sent to it, until every sender is
/// dropped or `stop` is cancelled. Either ends the stream and drops the
/// receiver, so the writer stops sending to this output. Chunks are hashed
/// as they're sent.
fn chunk_stream(
  receiver: UnboundedReceiver<Chunk>,
  stop: CancellationToken,
  hasher: Hasher,
) -> impl Stream<Item = Result<Chunk, std::io::Error>> + Send + 'static {
  futures::stream::unfold(receiver, move |mut receiver| {
    let stop = stop.clone();
    let hasher = hasher.clone();
    async move {
      let chunk = tokio::select! {
        chunk = receiver.recv() => chunk?,
        _ = stop.cancelled() => return None,
      };
      hash_chunk(&hasher, &chunk);
      Some((Ok(chunk), receiver))
    }
  })
}

async fn check_response(url: &str, response: reqwest::Result<reqwest::Response>) -> Result<(), String> {
  let response = response.map_err(|error| format!("{url}: {}", error_chain(&error)))?;
  let status = response.status();
  if status.is_success() {
    return Ok(());
  }
  let body: String = response.text().await.unwrap_or_default().chars().take(4096).collect();
  Err(format!("{url}: HTTP {status} {}", body.trim()))
}

fn create_parent(path: &Path) -> Result<(), String> {
  match path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
    Some(parent) => {
      std::fs::create_dir_all(parent).map_err(|error| format!("couldn't create {}: {error}", parent.display()))
    }
    None => Ok(()),
  }
}

/// Writes the recording to `path`, returning its Adler-32 if `hash`.
fn write_file(
  path: &str,
  config: &str,
  mut receiver: UnboundedReceiver<Chunk>,
  hash: bool,
) -> Result<Option<u32>, String> {
  let path = Path::new(path);
  create_parent(path)?;
  let mut config_path = path.as_os_str().to_owned();
  config_path.push(".json");
  let config_path = Path::new(&config_path);
  // Only the Main output was checked when choosing the recording number, so
  // another output's name may be taken. Then this output fails rather than
  // overwriting a recording.
  if let Some(existing) = [path, config_path].into_iter().find(|path| path.exists()) {
    return Err(format!("{} already exists", existing.display()));
  }
  std::fs::write(config_path, config).map_err(|error| format!("couldn't write {}: {error}", config_path.display()))?;
  let mut file = File::create(path).map_err(|error| format!("couldn't create {}: {error}", path.display()))?;
  let mut adler = hash.then(adler2::Adler32::new);
  while let Some(chunk) = receiver.blocking_recv() {
    file.write_all(&chunk).map_err(|error| format!("couldn't write {}: {error}", path.display()))?;
    if let Some(adler) = adler.as_mut() {
      adler.write_slice(&chunk);
    }
  }
  file.flush().map_err(|error| format!("couldn't write {}: {error}", path.display()))?;
  Ok(adler.map(|adler| adler.checksum()))
}

/// Streams the recording to `url`, returning its Adler-32 if `hash`.
async fn post_http(
  client: reqwest::Client,
  url: &str,
  config: &str,
  receiver: UnboundedReceiver<Chunk>,
  hash: bool,
) -> Result<Option<u32>, String> {
  let config_url = format!("{url}.json");
  let response = client
    .post(&config_url)
    .header(CONTENT_TYPE, "application/json")
    .body(config.to_string())
    .send()
    .await;
  check_response(&config_url, response).await?;

  // No length, so it's sent chunked until recording stops. The response
  // normally comes once the body ends; if it comes first (e.g. an error),
  // hyper would keep sending the body, so it's ended here.
  let stop = CancellationToken::new();
  let hasher = new_hasher(hash);
  let body = reqwest::Body::wrap_stream(chunk_stream(receiver, stop.clone(), hasher.clone()));
  let response = client.post(url).header(CONTENT_TYPE, "application/octet-stream").body(body).send().await;
  stop.cancel();
  check_response(url, response).await?;
  Ok(checksum(&hasher))
}

/// Where an output's Adler-32 goes: a Hash URL and its rendered name.
#[derive(Debug, Clone, PartialEq)]
struct HashTarget {
  target: Output,
  name: String,
}

/// Thoth's format: 8 lowercase hex digits.
fn hash_text(adler: u32) -> String {
  format!("{adler:08x}")
}

/// Writes or POSTs `adler` to `hash`. A hash file isn't overwritten, like
/// an output.
async fn store_hash(client: &reqwest::Client, hash: &HashTarget, adler: u32) -> Result<(), String> {
  match &hash.target {
    Output::File(_) => {
      let path = Path::new(&hash.name);
      create_parent(path)?;
      if path.exists() {
        return Err(format!("{} already exists", path.display()));
      }
      std::fs::write(path, hash_text(adler)).map_err(|error| format!("couldn't write {}: {error}", path.display()))
    }
    Output::Http(_) => {
      let request = client.post(&hash.name).header(CONTENT_TYPE, "text/plain").body(hash_text(adler));
      check_response(&hash.name, request.send().await).await
    }
  }
}

/// What writes an output.
enum Worker {
  Thread(JoinHandle<()>),
  Task(tokio::task::JoinHandle<()>),
}

/// An output's worker and the sender feeding it.
struct OutputWorker {
  name: String,
  sender: UnboundedSender<Chunk>,
  worker: Worker,
}

/// Logs how output `name` finished. Returns its hash, if it succeeded and
/// computed one.
fn log_result(api: ThalamusAPIThreadSafe, name: &str, result: Result<Option<u32>, String>) -> Option<u32> {
  match result {
    Ok(adler) => {
      report(api, &format!("finished {name}"));
      adler
    }
    Err(error) => {
      report(api, &format!("output failed, stopping it: {error}"));
      None
    }
  }
}

fn log_hash(api: ThalamusAPIThreadSafe, name: &str, hash: &HashTarget, result: Result<(), String>) {
  match result {
    Ok(()) => report(api, &format!("stored the hash of {name} at {}", hash.name)),
    Err(error) => report(api, &format!("couldn't store the hash of {name}: {error}")),
  }
}

fn start_output(
  api: ThalamusAPIThreadSafe,
  output: &Output,
  name: String,
  hash: Option<HashTarget>,
  config: Arc<String>,
  client: &reqwest::Client,
  runtime: &Handle,
) -> std::io::Result<OutputWorker> {
  let (sender, receiver) = unbounded_channel();
  let worker_name = name.clone();
  let client = client.clone();
  let worker = match output {
    Output::File(_) => {
      let runtime = runtime.clone();
      Worker::Thread(thread::Builder::new().name("STORAGE3 output".to_string()).spawn(move || {
        let adler = log_result(api, &worker_name, write_file(&worker_name, &config, receiver, hash.is_some()));
        if let (Some(hash), Some(adler)) = (hash, adler) {
          // Not a runtime thread, so it can block on one.
          log_hash(api, &worker_name, &hash, runtime.block_on(store_hash(&client, &hash, adler)));
        }
      })?)
    }
    Output::Http(_) => Worker::Task(runtime.spawn(async move {
      let result = post_http(client.clone(), &worker_name, &config, receiver, hash.is_some()).await;
      if let (Some(hash), Some(adler)) = (hash, log_result(api, &worker_name, result)) {
        log_hash(api, &worker_name, &hash, store_hash(&client, &hash, adler).await);
      }
    })),
  };
  Ok(OutputWorker { name, sender, worker })
}

/// Sends `chunk` to every output, dropping those that have failed (their
/// worker logged why and dropped its receiver). Returns whether this left
/// none.
fn send_to_outputs(outputs: &mut Vec<OutputWorker>, chunk: Chunk) -> bool {
  let before = outputs.len();
  outputs.retain(|output| output.sender.send(chunk.clone()).is_ok());
  outputs.is_empty() && before > 0
}

const ALL_FAILED: &str = "every output has failed, nothing more will be recorded";

/// A metric's key: the index of its group (0 for the queue stats, 1 for
/// events and log, 2 + i for source i) and what in the group it counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum MetricKey {
  Stat(usize, usize),
  Channel(usize, i32),
  Modality(usize, &'static str),
}

#[derive(Default)]
struct Metrics {
  /// Each metric's sum since the last publish and whether it's a rate
  /// (divided by the time since then when published).
  values: Vec<(f64, bool)>,
  names: Vec<String>,
  offsets: HashMap<MetricKey, usize>,
  changed: bool,
}

impl Metrics {
  fn add(&mut self, key: MetricKey, count: f64, is_rate: bool, name: impl FnOnce() -> String) {
    let offset = *self.offsets.entry(key).or_insert_with(|| {
      self.values.push((0.0, is_rate));
      self.names.push(name());
      self.changed = true;
      self.values.len() - 1
    });
    self.values[offset].0 += count;
  }
}

#[derive(Default)]
struct Counters {
  queued_records: u64,
  queued_bytes: u64,
  written_bytes: u64,
  max_queued_bytes: u64,
  sweeps: u64,
  sweep_time: Duration,
}

#[derive(Default)]
struct Queue {
  records: Vec<(u64, Vec<u8>)>,
  bytes: u64,
}

/// What the main thread, the data callbacks and the writer thread share.
#[derive(Default)]
struct Shared {
  running: AtomicBool,
  queue: Mutex<Queue>,
  condvar: Condvar,
  metrics: Mutex<Metrics>,
  counters: Mutex<Counters>,
  /// The current recording's Main output (get_current_file).
  current_file: Mutex<Option<String>>,
}

impl Shared {
  fn queue(&self, time_ns: u64, record: Vec<u8>) {
    if !self.running.load(Ordering::Acquire) {
      return;
    }
    let bytes = record.len() as u64;
    {
      let mut counters = self.counters.lock().unwrap();
      counters.queued_records += 1;
      counters.queued_bytes += bytes;
    }
    let mut queue = self.queue.lock().unwrap();
    queue.bytes += bytes;
    queue.records.push((time_ns, record));
    self.condvar.notify_one();
  }

  fn count(&self, key: MetricKey, count: usize, name: impl FnOnce() -> String) {
    self.metrics.lock().unwrap().add(key, count as f64, true, name);
  }

  fn reset(&self) {
    *self.queue.lock().unwrap() = Queue::default();
    *self.counters.lock().unwrap() = Counters::default();
    *self.metrics.lock().unwrap() = Metrics::default();
  }
}

/// A row of the Sources table.
#[derive(Debug, Clone, PartialEq)]
struct Source {
  node: String,
  time_series: bool,
  image: bool,
  motion: bool,
  text: bool,
}

fn string_at(state: &State, key: &str) -> Option<String> {
  match state.get(key) {
    Some(StateValue::String(value)) => Some(value),
    _ => None,
  }
}

fn bool_at(state: &State, key: &str, default: bool) -> bool {
  match state.get(key) {
    Some(StateValue::Bool(value)) => value,
    Some(StateValue::Int(value)) => value != 0,
    _ => default,
  }
}

fn read_sources(state: &State) -> Vec<Source> {
  let Some(StateValue::List(list)) = state.get("Sources") else {
    return Vec::new();
  };
  (&list)
    .into_iter()
    .filter_map(|entry| match entry.val {
      StateValue::Dict(source) => Some(Source {
        node: string_at(&source, "Node").unwrap_or_default(),
        time_series: bool_at(&source, "Time Series", true),
        image: bool_at(&source, "Image", true),
        motion: bool_at(&source, "Motion", true),
        text: bool_at(&source, "Text", true),
      }),
      _ => None,
    })
    .filter(|source| !source.node.is_empty())
    .collect()
}

/// A row of the Outputs table.
#[derive(Debug, Clone, PartialEq)]
struct OutputConfig {
  url: String,
  main: bool,
  /// Empty for no hash.
  hash_url: String,
}

fn output_rows(state: &State) -> Vec<State> {
  let Some(StateValue::List(list)) = state.get("Outputs") else {
    return Vec::new();
  };
  (&list)
    .into_iter()
    .filter_map(|entry| match entry.val {
      StateValue::Dict(row) => Some(row),
      _ => None,
    })
    .collect()
}

fn read_outputs(state: &State) -> Vec<OutputConfig> {
  output_rows(state)
    .iter()
    .map(|row| OutputConfig {
      url: string_at(row, "URL").unwrap_or_default(),
      main: bool_at(row, "Main", false),
      hash_url: string_at(row, "Hash URL").unwrap_or_default(),
    })
    .collect()
}

/// Which output should be Main: `preferred` (just set Main) if it's one of
/// them, else the first marked Main, else the first.
fn main_output_index(mains: &[bool], preferred: Option<usize>) -> Option<usize> {
  if mains.is_empty() {
    return None;
  }
  preferred.filter(|&i| i < mains.len()).or_else(|| mains.iter().position(|&main| main)).or(Some(0))
}

/// Makes exactly one output Main (see main_output_index).
fn normalize_main(state: &State, preferred: Option<&State>) {
  let rows = output_rows(state);
  let mains: Vec<bool> = rows.iter().map(|row| bool_at(row, "Main", false)).collect();
  let preferred = preferred.and_then(|preferred| rows.iter().position(|row| row == preferred));
  let Some(main) = main_output_index(&mains, preferred) else {
    return;
  };
  for (i, row) in rows.iter().enumerate() {
    let want = i == main;
    // Only changes are set: each set calls the state callback, which
    // normalizes again.
    if mains[i] != want || !matches!(row.get("Main"), Some(StateValue::Bool(_))) {
      row.set("Main", want);
    }
  }
}

fn state_to_json(value: StateValue) -> serde_json::Value {
  match value {
    StateValue::Bool(value) => value.into(),
    StateValue::Int(value) => value.into(),
    StateValue::Float(value) => {
      serde_json::Number::from_f64(value).map(serde_json::Value::Number).unwrap_or(serde_json::Value::Null)
    }
    StateValue::String(value) => value.into(),
    StateValue::Null => serde_json::Value::Null,
    StateValue::Dict(dict) => serde_json::Value::Object(
      (&dict)
        .into_iter()
        .map(|entry| {
          let key = match entry.key {
            StateKey::String(key) => key,
            StateKey::Int(key) => key.to_string(),
          };
          (key, state_to_json(entry.val))
        })
        .collect(),
    ),
    StateValue::List(list) => {
      serde_json::Value::Array((&list).into_iter().map(|entry| state_to_json(entry.val)).collect())
    }
  }
}

/// Like STORAGE2: stores the recording number in the Metadata table's "Rec"
/// row (adding it if missing) and returns the table's pairs.
fn update_metadata(api: ThalamusAPI, state: &State, rec: u32) -> Vec<(String, Option<MetadataValue>)> {
  state.set("rec", i64::from(rec));
  let rec_value = Some(MetadataValue::Integral(i64::from(rec)));
  let new_rec_row = || {
    let row = State::make_dict(api);
    row.set("Key", "Rec");
    row.set("Value", i64::from(rec));
    row
  };
  let mut pairs = Vec::new();
  match state.get("Metadata") {
    Some(StateValue::List(list)) => {
      let mut found = false;
      for entry in &list {
        let StateValue::Dict(row) = entry.val else {
          continue;
        };
        let key = string_at(&row, "Key").unwrap_or_default();
        if key == "Rec" {
          row.set("Value", i64::from(rec));
          found = true;
          pairs.push((key, rec_value.clone()));
          continue;
        }
        let value = match row.get("Value") {
          Some(StateValue::String(text)) => Some(MetadataValue::Text(text)),
          Some(StateValue::Float(decimal)) => Some(MetadataValue::Decimal(decimal)),
          Some(StateValue::Int(integral)) => Some(MetadataValue::Integral(integral)),
          Some(StateValue::Bool(value)) => Some(MetadataValue::Integral(i64::from(value))),
          _ => None,
        };
        pairs.push((key, value));
      }
      if !found {
        pairs.push(("Rec".to_string(), rec_value));
        list.push_state(&new_rec_row());
      }
    }
    _ => {
      pairs.push(("Rec".to_string(), rec_value));
      let list = State::make_list(api);
      list.push_state(&new_rec_row());
      state.set("Metadata", StateValue::List(list));
    }
  }
  pairs
}

/// Everything the writer thread needs to start a recording.
struct Recording {
  shared: Arc<Shared>,
  api: ThalamusAPIThreadSafe,
  state: MainThreadOnly<State>,
  node_name: String,
  outputs: Vec<Output>,
  /// Each output's Hash URL, if it has one.
  hashes: Vec<Option<Output>>,
  /// Index in outputs of the Main output.
  main: usize,
  config: String,
  runtime: Handle,
}

fn run_writer(recording: Recording) {
  let Recording { shared, api, state, node_name, outputs, hashes, main, config, runtime } = recording;
  let start = Local::now();
  let client = match http_client() {
    Ok(client) => client,
    Err(error) => {
      report(api, &error);
      return;
    }
  };
  let rec = next_rec(&outputs[main], &start, |output, name| output_exists(api, output, name, &client, &runtime));
  let names: Vec<String> = outputs.iter().map(|output| recording_name(output.template(), rec, &start)).collect();
  // The Main output: a file's absolute path, or a URL.
  let current_file = match &outputs[main] {
    Output::File(_) => std::path::absolute(&names[main])
      .map(|path| path.to_string_lossy().into_owned())
      .unwrap_or_else(|_| names[main].clone()),
    Output::Http(_) => names[main].clone(),
  };
  *shared.current_file.lock().unwrap() = Some(current_file);

  // The Metadata table is state, so it's read and updated on the main
  // thread.
  let (sender, receiver) = mpsc::channel();
  api.post_to_main(move |token| {
    let state = state.take(token);
    let _ = sender.send(update_metadata(api.thread_unsafe(token), &state, rec));
  });
  let mut pairs = loop {
    match receiver.recv_timeout(POLL_INTERVAL) {
      Ok(pairs) => break pairs,
      Err(RecvTimeoutError::Timeout) if shared.running.load(Ordering::Acquire) => {}
      Err(_) => return,
    }
  };
  // Thalamus's under STORAGE2's keys (left out if Thalamus predates the
  // query), then this plugin's.
  let thalamus = [
    ("Build Type", api.thalamus_build_type()),
    ("Version", api.thalamus_version()),
    ("Commit", api.thalamus_commit()),
  ];
  let contrib = [
    ("Contrib Build Type", Some(build_info::BUILD_TYPE)),
    ("Contrib Version", Some(build_info::VERSION)),
    ("Contrib Commit", Some(build_info::COMMIT)),
  ];
  for (key, value) in thalamus.into_iter().chain(contrib) {
    if let Some(value) = value {
      pairs.push((key.to_string(), Some(MetadataValue::Text(value.to_string()))));
    }
  }

  let config = Arc::new(config);
  let mut output_workers: Vec<OutputWorker> = Vec::new();
  for ((output, name), hash) in outputs.iter().zip(names).zip(hashes) {
    report(api, &format!("recording to {name}"));
    let hash = hash.map(|target| HashTarget { name: recording_name(target.template(), rec, &start), target });
    match start_output(api, output, name, hash, config.clone(), &client, &runtime) {
      Ok(worker) => output_workers.push(worker),
      Err(error) => report(api, &format!("couldn't start an output thread: {error}")),
    }
  }

  let mut buffer = Vec::new();
  let time_ns = api.time().as_nanos() as u64;
  storage_record::frame(&mut buffer, &storage_record::metadata(&node_name, time_ns, &pairs));
  if send_to_outputs(&mut output_workers, Bytes::from(buffer)) {
    report(api, ALL_FAILED);
  }

  let sweep = |records: Vec<(u64, Vec<u8>)>, output_workers: &mut Vec<OutputWorker>| {
    if records.is_empty() {
      return;
    }
    let started = Instant::now();
    let mut records = records;
    records.sort_by_key(|(time, _)| *time);
    let mut buffer = Vec::with_capacity(records.iter().map(|(_, record)| record.len() + 8).sum());
    for (_, record) in &records {
      storage_record::frame(&mut buffer, record);
    }
    let bytes = buffer.len() as u64;
    if send_to_outputs(output_workers, Bytes::from(buffer)) {
      report(api, ALL_FAILED);
    }
    let mut counters = shared.counters.lock().unwrap();
    counters.written_bytes += bytes;
    counters.sweeps += 1;
    counters.sweep_time += started.elapsed();
  };

  let take = |wait: bool| {
    let mut queue = shared.queue.lock().unwrap();
    if wait {
      queue = shared
        .condvar
        .wait_timeout_while(queue, Duration::from_secs(1), |queue| {
          queue.records.is_empty() && shared.running.load(Ordering::Acquire)
        })
        .unwrap()
        .0;
    }
    let bytes = std::mem::take(&mut queue.bytes);
    let records = std::mem::take(&mut queue.records);
    drop(queue);
    let mut counters = shared.counters.lock().unwrap();
    counters.max_queued_bytes = counters.max_queued_bytes.max(bytes);
    records
  };

  while shared.running.load(Ordering::Acquire) {
    let records = take(true);
    sweep(records, &mut output_workers);
  }
  // What arrived before recording stopped.
  let records = take(false);
  sweep(records, &mut output_workers);

  // Ends each output's stream; they finish on their own.
  for OutputWorker { name, sender, worker } in output_workers {
    drop(sender);
    let panicked = match worker {
      Worker::Thread(thread) => thread.join().is_err(),
      Worker::Task(task) => runtime.block_on(task).is_err(),
    };
    if panicked {
      report(api, &format!("output for {name} panicked"));
    }
  }
}

/// Queues the parts of `data` that `source` records.
fn record_data(shared: &Shared, source: &Source, index: usize, count_metrics: bool, data: &dyn NodeData) {
  if !shared.running.load(Ordering::Acquire) {
    return;
  }
  let node = &source.node;
  let time_ns = data.time().as_nanos() as u64;
  let count = |modality: &'static str| {
    if count_metrics {
      shared.count(MetricKey::Modality(index, modality), 1, || format!("{node} {modality}"));
    }
  };
  if source.time_series && let Some(analog) = data.analog() {
    shared.queue(time_ns, storage_record::analog(node, time_ns, analog));
    if count_metrics {
      for channel in 0..analog.num_channels() {
        shared.count(MetricKey::Channel(index, channel), analog.count(channel), || {
          format!("{node}({})", analog.name(channel))
        });
      }
    }
  }
  if source.image && let Some(image) = data.image() {
    shared.queue(time_ns, storage_record::image(node, time_ns, image));
    count("Image");
  }
  if source.motion && let Some(mocap) = data.mocap() {
    shared.queue(time_ns, storage_record::mocap(node, time_ns, mocap));
    count("Motion");
  }
  if source.text && let Some(text) = data.text() {
    shared.queue(time_ns, storage_record::text(node, time_ns, text));
    count("Text");
  }
  // Like STORAGE2, blobs are always recorded.
  if let Some(blob) = data.blob() {
    shared.queue(time_ns, storage_record::blob(node, time_ns, blob));
    count("Blob");
  }
}

/// The metrics this node publishes as analog data, one sample per channel
/// per second.
struct MetricsData {
  values: Vec<f64>,
  names: Vec<String>,
  changed: bool,
  time: Duration,
}

impl NodeData for MetricsData {
  fn time(&self) -> Duration {
    self.time
  }
  fn analog(&self) -> Option<&dyn AnalogData> {
    Some(self)
  }
}

impl AnalogData for MetricsData {
  fn data(&self, channel: i32) -> &[f64] {
    std::slice::from_ref(&self.values[channel as usize])
  }
  fn num_channels(&self) -> i32 {
    self.values.len() as i32
  }
  fn sample_interval(&self, _channel: i32) -> Duration {
    STATS_INTERVAL
  }
  fn name(&self, channel: i32) -> &str {
    &self.names[channel as usize]
  }
  fn channels_changed(&self) -> bool {
    self.changed
  }
}

struct Inner {
  api: ThalamusAPI,
  token: MainThreadToken,
  node_token: NodeToken,
  state: State,
  shared: Arc<Shared>,
  state_connection: Option<OnDrop>,
  message_connections: Vec<OnDrop>,
  source_connections: Vec<OnDrop>,
  /// Each source's data subscription, replaced when the node its name
  /// resolves to changes.
  data_connections: Rc<RefCell<Vec<Option<OnDrop>>>>,
  writer: Option<JoinHandle<()>>,
  timer: Timer,
  last_publish: Instant,
}

pub struct Storage3Node {
  inner: Rc<RefCell<Inner>>,
}

/// Stops recording: the writer thread finishes the outputs on its own, and
/// is joined if `join`.
fn stop(inner: &Rc<RefCell<Inner>>, join: bool) {
  let (writer, data_connections) = {
    let mut borrow = inner.borrow_mut();
    borrow.shared.running.store(false, Ordering::Release);
    borrow.shared.condvar.notify_all();
    *borrow.shared.current_file.lock().unwrap() = None;
    let source_connections = std::mem::take(&mut borrow.source_connections);
    drop(source_connections);
    (if join { borrow.writer.take() } else { None }, borrow.data_connections.clone())
  };
  data_connections.borrow_mut().clear();
  if let Some(writer) = writer {
    let _ = writer.join();
  }
}

fn fail_start(inner: &Rc<RefCell<Inner>>, message: &str) {
  let borrow = inner.borrow();
  report(borrow.api.thread_safe(), message);
  borrow.api.show_dialog("STORAGE3", message, DialogType::Error);
  // Not from inside the state callback that started it.
  let state = borrow.state.clone();
  deferred!(state.set("Running", false));
}

fn start(inner: &Rc<RefCell<Inner>>) {
  stop(inner, true);

  let (api, token, state, shared) = {
    let borrow = inner.borrow();
    (borrow.api, borrow.token, borrow.state.clone(), borrow.shared.clone())
  };

  let configs = read_outputs(&state);
  let mains: Vec<bool> = configs.iter().map(|config| config.main).collect();
  let Some(main_config) = main_output_index(&mains, None) else {
    fail_start(inner, "No outputs");
    return;
  };
  let mut outputs = Vec::new();
  let mut hashes = Vec::new();
  let mut main = None;
  for (i, config) in configs.iter().enumerate() {
    let hash = match config.hash_url.trim() {
      "" => Ok(None),
      url => parse_output(url).map(Some).map_err(|error| format!("Hash URL: {error}")),
    };
    match parse_output(&config.url).and_then(|output| Ok((output, hash?))) {
      Ok((output, hash)) => {
        if i == main_config {
          main = Some(outputs.len());
        }
        outputs.push(output);
        hashes.push(hash);
      }
      Err(error) if i == main_config => {
        fail_start(inner, &format!("The Main output is invalid: {error}"));
        return;
      }
      Err(error) => report(api.thread_safe(), &format!("skipping output: {error}")),
    }
  }
  let Some(main) = main else {
    fail_start(inner, "No Main output");
    return;
  };
  let Some(runtime) = api.tokio().as_ref().map(|runtime| runtime.handle().clone()) else {
    fail_start(inner, "the tokio runtime isn't running");
    return;
  };

  shared.reset();
  shared.running.store(true, Ordering::Release);

  let node_name = string_at(&state, "name").unwrap_or_default();
  let config = state_to_json(StateValue::Dict(state.root())).to_string();

  let sources = read_sources(&state);
  let data_connections = inner.borrow().data_connections.clone();
  *data_connections.borrow_mut() = sources.iter().map(|_| None).collect();
  let mut source_connections = Vec::new();
  for (i, source) in sources.into_iter().enumerate() {
    let index = i + 2;
    // A node recording itself doesn't count its own metrics.
    let count_metrics = source.node != node_name;
    let shared = shared.clone();
    let data_connections = Rc::downgrade(&data_connections);
    let selector = NodeSelector::Name(source.node.clone());
    source_connections.push(api.get_node(selector, move |ext_node| {
      let shared = shared.clone();
      let source = source.clone();
      let connection = ext_node.subscribe_multithreaded(move |ext_node| {
        record_data(&shared, &source, index, count_metrics, &ext_node.data());
      });
      if let Some(data_connections) = data_connections.upgrade() {
        data_connections.borrow_mut()[i] = Some(connection);
      }
    }));
  }

  let recording = Recording {
    shared,
    api: api.thread_safe(),
    state: MainThreadOnly::new(state, api, token),
    node_name,
    outputs,
    hashes,
    main,
    config,
    runtime,
  };
  let writer = thread::Builder::new().name("STORAGE3".to_string()).spawn(move || run_writer(recording));
  let mut borrow = inner.borrow_mut();
  borrow.source_connections = source_connections;
  match writer {
    Ok(writer) => borrow.writer = Some(writer),
    Err(error) => {
      drop(borrow);
      stop(inner, false);
      fail_start(inner, &format!("couldn't start the writer thread: {error}"));
      return;
    }
  }
  borrow.last_publish = Instant::now();
  drop(borrow);
  schedule_stats(Rc::downgrade(inner));
}

fn schedule_stats(weak: Weak<RefCell<Inner>>) {
  let Some(inner) = weak.upgrade() else {
    return;
  };
  let borrow = inner.borrow();
  borrow.timer.sleep_callback(STATS_INTERVAL, move |error| {
    if !error.aborted() {
      publish_stats(&weak);
    }
  });
}

/// Publishes the metrics, like STORAGE2's on_stats_timer.
fn publish_stats(weak: &Weak<RefCell<Inner>>) {
  let Some(inner) = weak.upgrade() else {
    return;
  };
  let data = {
    let mut borrow = inner.borrow_mut();
    if !borrow.shared.running.load(Ordering::Acquire) {
      return;
    }
    let counters = std::mem::take(&mut *borrow.shared.counters.lock().unwrap());
    let mut metrics = borrow.shared.metrics.lock().unwrap();
    let stats = [
      ("Output Queue Count", counters.queued_records as f64, true),
      ("Output Queue Bytes", counters.queued_bytes as f64, true),
      ("Written Bytes", counters.written_bytes as f64, true),
      ("Max Queued Bytes", counters.max_queued_bytes as f64, false),
      (
        "Average Sweep (ns)",
        if counters.sweeps > 0 { counters.sweep_time.as_nanos() as f64 / counters.sweeps as f64 } else { 0.0 },
        false,
      ),
    ];
    for (i, (name, value, is_rate)) in stats.into_iter().enumerate() {
      metrics.add(MetricKey::Stat(0, i), value, is_rate, || name.to_string());
    }
    let now = Instant::now();
    let elapsed = (now - borrow.last_publish).as_secs_f64().max(f64::EPSILON);
    let values = metrics.values.iter().map(|&(value, is_rate)| if is_rate { value / elapsed } else { value }).collect();
    let data = MetricsData {
      values,
      names: metrics.names.clone(),
      changed: std::mem::take(&mut metrics.changed),
      time: borrow.api.time(),
    };
    for value in &mut metrics.values {
      value.0 = 0.0;
    }
    drop(metrics);
    borrow.last_publish = now;
    data
  };
  let (api, node_token) = {
    let borrow = inner.borrow();
    (borrow.api, borrow.node_token.clone())
  };
  let _ = api.ready(&data, &node_token);
  schedule_stats(weak.clone());
}

fn process(shared: &Shared, api: ThalamusAPI, handle: Request, request: Json) {
  let request = serde_json::from_str::<serde_json::Value>(&request.to_string()).unwrap_or_default();
  let request_type = match &request {
    serde_json::Value::String(s) => Some(s.as_str()),
    serde_json::Value::Object(o) => o.get("type").and_then(|t| t.as_str()),
    _ => None,
  };
  let response = match request_type {
    // STORAGE2's request: the Main output, a path or a URL.
    Some("get_current_file") => match &*shared.current_file.lock().unwrap() {
      Some(name) => serde_json::Value::String(name.clone()),
      None => serde_json::Value::Null,
    },
    _ => serde_json::Value::Null,
  };
  handle.respond(&Json::from_string(api, &response.to_string()));
}

impl NodeConsts for Storage3Node {
  const MODALITIES: u32 = THALAMUS_MODALITY_ANALOG;
}

impl Node for Storage3Node {
  fn new(api: ThalamusAPI, node_token: NodeToken, state: State, token: MainThreadToken) -> Self {
    let shared = Arc::new(Shared::default());
    let inner = Rc::new(RefCell::new(Inner {
      api,
      token,
      node_token: node_token.clone(),
      state: state.clone(),
      shared: shared.clone(),
      state_connection: None,
      message_connections: Vec::new(),
      source_connections: Vec::new(),
      data_connections: Rc::new(RefCell::new(Vec::new())),
      writer: None,
      timer: api.create_timer(),
      last_publish: Instant::now(),
    }));

    let mut message_connections = Vec::new();
    let events_shared = shared.clone();
    let events = api.subscribe_events(move |event| {
      events_shared.queue(event.time.as_nanos() as u64, storage_record::event(event));
      if events_shared.running.load(Ordering::Acquire) {
        events_shared.count(MetricKey::Stat(1, 0), 1, || "Events".to_string());
      }
    });
    let log_shared = shared.clone();
    let log = api.subscribe_log(move |log| {
      log_shared.queue(log.time.as_nanos() as u64, storage_record::log(log));
      if log_shared.running.load(Ordering::Acquire) {
        log_shared.count(MetricKey::Stat(1, 1), 1, || "Log".to_string());
      }
    });
    match (events, log) {
      (Some(events), Some(log)) => message_connections.extend([events, log]),
      _ => report(api.thread_safe(), "this Thalamus can't share events and log messages, so they won't be recorded"),
    }
    inner.borrow_mut().message_connections = message_connections;

    let weak = Rc::downgrade(&inner);
    let own_state = state.clone();
    let state_connection = state.connect(move |source: State, _action: StateAction, key: StateKey, value: StateValue| {
      let Some(inner) = weak.upgrade() else {
        return;
      };
      if source != own_state {
        // A change in the Outputs list or one of its rows.
        let outputs = match own_state.get("Outputs") {
          Some(StateValue::List(outputs)) => outputs,
          _ => return,
        };
        let in_outputs = source == outputs || source.parent().is_some_and(|parent| parent == outputs);
        if !in_outputs {
          return;
        }
        let made_main = source != outputs
          && matches!(&key, StateKey::String(key) if key == "Main")
          && matches!(value, StateValue::Bool(true));
        // Its sets re-enter this callback, which normalizes again; that
        // converges, since each pass only sets rows that differ.
        normalize_main(&own_state, made_main.then_some(&source));
      }
      if !matches!(&key, StateKey::String(key) if key == "Running") {
        return;
      }
      if matches!(value, StateValue::Bool(true)) {
        start(&inner);
      } else {
        stop(&inner, false);
      }
    });
    inner.borrow_mut().state_connection = Some(state_connection);

    let process_shared = shared.clone();
    node_token.set_process(move |handle, request| process(&process_shared, api, handle, request));

    let weak = Rc::downgrade(&inner);
    node_token.set_predrop(move |token| {
      if let Some(inner) = weak.upgrade() {
        let mut borrow = inner.borrow_mut();
        borrow.message_connections.clear();
        borrow.source_connections.clear();
        let data_connections = borrow.data_connections.clone();
        drop(borrow);
        data_connections.borrow_mut().clear();
      }
      token.ready();
    });

    state.recap();
    Storage3Node { inner }
  }
}

impl Drop for Storage3Node {
  fn drop(&mut self) {
    stop(&self.inner, true);
    let mut borrow = self.inner.borrow_mut();
    borrow.state_connection = None;
    borrow.message_connections.clear();
    borrow.state.set("Running", false);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use chrono::TimeZone;

  fn time() -> DateTime<Local> {
    Local.with_ymd_and_hms(2026, 10, 22, 13, 5, 9).unwrap()
  }

  #[test]
  fn outputs_are_paths_unless_they_name_a_protocol() {
    assert_eq!(parse_output("test.tha"), Ok(Output::File("test.tha".to_string())));
    assert_eq!(parse_output(r"C:\data\test.tha"), Ok(Output::File(r"C:\data\test.tha".to_string())));
    assert_eq!(parse_output("file:///C:/data/test.tha"), Ok(Output::File("C:/data/test.tha".to_string())));
    assert_eq!(parse_output("file:///home/me/test.tha"), Ok(Output::File("/home/me/test.tha".to_string())));
    assert_eq!(parse_output("FILE://test.tha"), Ok(Output::File("test.tha".to_string())));
    assert_eq!(
      parse_output(" https://thoth/files/data/test.tha "),
      Ok(Output::Http("https://thoth/files/data/test.tha".to_string()))
    );
    assert_eq!(parse_output("http://thoth/x"), Ok(Output::Http("http://thoth/x".to_string())));
    assert!(parse_output("ftp://thoth/x").is_err());
    assert!(parse_output("file://").is_err());
    assert!(parse_output("  ").is_err());
  }

  #[test]
  fn names_are_rendered_like_storage2() {
    assert_eq!(recording_name("test.tha", 1, &time()), "test.tha.20261022.1");
    assert_eq!(recording_name("data/%Y-%m-%d/rec{{rec}}.tha", 12, &time()), "data/2026-10-22/rec012.tha.20261022.12");
    assert_eq!(recording_name("{{ rec }}_{{other}}_%H%M", 3, &time()), "003_{{other}}_1305.20261022.3");
    // Not a strftime template: % fields are kept.
    assert_eq!(
      recording_name("https://thoth/files/my%20data/t.tha", 2, &time()),
      "https://thoth/files/my%20data/t.tha.20261022.2"
    );
  }

  #[test]
  fn the_recording_number_is_the_first_free_for_the_main_output() {
    let main = Output::File("a.tha".to_string());
    // Other outputs' recordings don't matter.
    let existing = ["a.tha.20261022.1", "http://h/files/b.tha.20261022.2", "a.tha.20261022.3", "a.tha.20261022.2"];
    assert_eq!(next_rec(&main, &time(), |_, name| existing.contains(&name)), 4);
    assert_eq!(next_rec(&main, &time(), |_, _| false), 1);
  }

  #[test]
  fn exactly_one_output_is_main() {
    assert_eq!(main_output_index(&[], None), None);
    // The first output becomes Main when none is.
    assert_eq!(main_output_index(&[false], None), Some(0));
    assert_eq!(main_output_index(&[false, false], None), Some(0));
    assert_eq!(main_output_index(&[false, true, true], None), Some(1));
    // One just made Main wins.
    assert_eq!(main_output_index(&[true, true], Some(1)), Some(1));
    assert_eq!(main_output_index(&[true, false], Some(5)), Some(0));
  }

  #[test]
  fn chunk_streams_end_when_the_sender_is_dropped() {
    use futures::StreamExt;
    let (sender, receiver) = unbounded_channel();
    sender.send(Bytes::from_static(b"Wiki")).unwrap();
    sender.send(Bytes::new()).unwrap();
    sender.send(Bytes::from_static(b"pedia")).unwrap();
    drop(sender);
    let chunks: Vec<Chunk> =
      futures::executor::block_on(chunk_stream(receiver, CancellationToken::new(), new_hasher(false)).map(Result::unwrap).collect());
    assert_eq!(chunks.concat(), b"Wikipedia");
  }

  #[test]
  fn stopping_a_chunk_stream_drops_its_receiver() {
    use futures::StreamExt;
    let (sender, receiver) = unbounded_channel();
    let stop = CancellationToken::new();
    let mut stream = Box::pin(chunk_stream(receiver, stop.clone(), new_hasher(false)));
    sender.send(Bytes::from_static(b"a")).unwrap();
    futures::executor::block_on(async {
      assert_eq!(stream.next().await.unwrap().unwrap(), Bytes::from_static(b"a"));
      stop.cancel();
      assert!(stream.next().await.is_none());
    });
    assert!(sender.send(Bytes::from_static(b"b")).is_err());
  }

  #[test]
  fn file_outputs_write_the_config_and_every_chunk() {
    let dir = std::env::temp_dir().join(format!("storage3_test_{}", std::process::id()));
    let path = dir.join("sub").join("test.tha");
    let (sender, receiver) = unbounded_channel();
    sender.send(Bytes::from_static(&[1, 2])).unwrap();
    sender.send(Bytes::from_static(&[3])).unwrap();
    drop(sender);
    assert_eq!(write_file(path.to_str().unwrap(), "{}", receiver, false).unwrap(), None);
    assert_eq!(std::fs::read(&path).unwrap(), vec![1, 2, 3]);
    assert_eq!(std::fs::read_to_string(dir.join("sub").join("test.tha.json")).unwrap(), "{}");
    std::fs::remove_dir_all(&dir).unwrap();
  }

  #[test]
  fn outputs_hash_what_they_write() {
    let dir = std::env::temp_dir().join(format!("storage3_hash_test_{}", std::process::id()));
    let path = dir.join("test.tha");
    let (sender, receiver) = unbounded_channel();
    sender.send(Bytes::from_static(b"Wiki")).unwrap();
    sender.send(Bytes::from_static(b"pedia")).unwrap();
    drop(sender);
    // zlib.adler32(b"Wikipedia")
    assert_eq!(write_file(path.to_str().unwrap(), "{}", receiver, true).unwrap(), Some(0x11e6_0398));
    assert_eq!(hash_text(0x11e6_0398), "11e60398");
    assert_eq!(hash_text(0xff), "000000ff");

    let hash = HashTarget { target: Output::File(String::new()), name: dir.join("sub").join("hash").to_string_lossy().into_owned() };
    let client = reqwest::Client::new();
    futures::executor::block_on(store_hash(&client, &hash, 0x11e6_0398)).unwrap();
    assert_eq!(std::fs::read_to_string(&hash.name).unwrap(), "11e60398");
    // Not overwritten.
    assert!(futures::executor::block_on(store_hash(&client, &hash, 1)).is_err());
    assert_eq!(std::fs::read_to_string(&hash.name).unwrap(), "11e60398");
    std::fs::remove_dir_all(&dir).unwrap();
  }

  #[test]
  fn http_bodies_are_hashed_as_sent() {
    use futures::StreamExt;
    let (sender, receiver) = unbounded_channel();
    sender.send(Bytes::from_static(b"Wiki")).unwrap();
    sender.send(Bytes::from_static(b"pedia")).unwrap();
    drop(sender);
    let hasher = new_hasher(true);
    let chunks: Vec<Chunk> = futures::executor::block_on(
      chunk_stream(receiver, CancellationToken::new(), hasher.clone()).map(Result::unwrap).collect(),
    );
    assert_eq!(chunks.concat(), b"Wikipedia");
    assert_eq!(checksum(&hasher), Some(0x11e6_0398));
    assert_eq!(checksum(&new_hasher(false)), None);
  }

  #[test]
  fn file_outputs_never_overwrite() {
    let dir = std::env::temp_dir().join(format!("storage3_overwrite_test_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let write = |path: &std::path::Path| {
      let (sender, receiver) = unbounded_channel();
      sender.send(Bytes::from_static(b"new")).unwrap();
      drop(sender);
      write_file(path.to_str().unwrap(), "{}", receiver, false)
    };

    // An existing recording is kept.
    let recording = dir.join("a.tha");
    std::fs::write(&recording, b"old").unwrap();
    assert!(write(&recording).is_err());
    assert_eq!(std::fs::read(&recording).unwrap(), b"old");
    assert!(!dir.join("a.tha.json").exists());

    // So is an existing config, and no recording is started beside it.
    let recording = dir.join("b.tha");
    std::fs::write(dir.join("b.tha.json"), b"old").unwrap();
    assert!(write(&recording).is_err());
    assert_eq!(std::fs::read(dir.join("b.tha.json")).unwrap(), b"old");
    assert!(!recording.exists());

    std::fs::remove_dir_all(&dir).unwrap();
  }

  #[test]
  fn a_failing_output_is_dropped_and_the_others_keep_receiving() {
    let (good_sender, mut good_receiver) = unbounded_channel();
    let (bad_sender, bad_receiver) = unbounded_channel::<Chunk>();
    drop(bad_receiver);
    let idle = || Worker::Thread(thread::spawn(|| {}));
    let mut outputs = vec![
      OutputWorker { name: "bad".to_string(), sender: bad_sender, worker: idle() },
      OutputWorker { name: "good".to_string(), sender: good_sender, worker: idle() },
    ];
    send_to_outputs(&mut outputs, Bytes::from_static(&[9]));
    assert_eq!(outputs.iter().map(|output| output.name.as_str()).collect::<Vec<_>>(), ["good"]);
    assert_eq!(good_receiver.blocking_recv().unwrap(), Bytes::from_static(&[9]));
  }
}
