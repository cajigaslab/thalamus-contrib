// Publishes the StorageRecords in "File": a regular file such as a STORAGE3
// recording, a character device, or a serial port. Records are framed as in
// storage_record.rs. A record
// whose time is zero is published as soon as it's read. Other records are
// paced: the first is published at once and the rest after the time between
// their time field and the first's.
use crate::api::{
  AnalogData, AnalogEncoding, AnalogFormat, ImageData, ImageFormat, MainThreadOnly, MainThreadToken, MocapData, Node,
  NodeConsts, NodeData, NodeToken, OffMainSignaler, OnDrop, State, StateAction, StateKey, StateValue,
  THALAMUS_MODALITY_ANALOG, THALAMUS_MODALITY_IMAGE, THALAMUS_MODALITY_MOCAP, THALAMUS_MODALITY_TEXT,
  ThalamusAPI, ThalamusAPIThreadSafe, ThalamusMocapSegment, TextData,
};
use crate::serial_config::{SerialSettings, open_serial, serial_settings};
use crate::storage_record::proto;
use crate::storage_record::proto::storage_record::Body;
use prost::Message;
use std::cell::RefCell;
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, ErrorKind, Read};
use std::ops::Range;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

// Longer than any record, so a corrupt size is an error.
const MAX_RECORD_SIZE: u64 = 1 << 30;
// Records read ahead of the pacing.
const READ_AHEAD: usize = 16;
// A read of a character device can't be interrupted, so don't wait for it
// forever. Serial reads end within SERIAL_TIMEOUT.
const READER_EXIT_TIMEOUT: Duration = Duration::from_secs(1);

// COM3 or \\.\COM3.
#[cfg(windows)]
fn is_serial_port(path: &str) -> bool {
  let name = path.strip_prefix(r"\\.\").unwrap_or(path);
  name.len() > 3
    && name.is_char_boundary(3)
    && name[..3].eq_ignore_ascii_case("com")
    && name[3..].bytes().all(|byte| byte.is_ascii_digit())
}

// Without O_NONBLOCK, opening a serial port can wait for carrier detect.
#[cfg(unix)]
fn is_serial_port(path: &str) -> bool {
  use std::io::IsTerminal;
  use std::os::unix::fs::OpenOptionsExt;
  std::fs::OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
    .open(path)
    .is_ok_and(|file| file.is_terminal())
}

#[derive(Debug)]
enum ReadError {
  Stopped,
  Io(io::Error),
  Truncated,
  // A serial line went idle inside a record. A size read from the wrong offset
  // can look valid, so this is how joining mid record is noticed. The next
  // byte starts a record.
  Idle,
  // Framing is lost, but a serial stream can recover.
  BadFrame(String),
}

impl std::fmt::Display for ReadError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      ReadError::Stopped => write!(f, "stopped"),
      ReadError::Io(e) => write!(f, "read failed: {e}"),
      ReadError::Truncated => write!(f, "the file ends in the middle of a record"),
      ReadError::Idle => write!(f, "the line went idle in the middle of a record"),
      ReadError::BadFrame(e) => write!(f, "{e}"),
    }
  }
}

fn is_timeout(error: &io::Error) -> bool {
  matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted)
}

// Fills `buffer`. Returns false if the source ended before the first byte.
// `in_record` says a record has already started; on a serial source, timing
// out then or after the first byte is an error.
fn read_full(
  source: &mut dyn Read,
  buffer: &mut [u8],
  serial: bool,
  in_record: bool,
  stop: &dyn Fn() -> bool,
) -> Result<bool, ReadError> {
  let mut filled = 0;
  while filled < buffer.len() {
    match source.read(&mut buffer[filled..]) {
      Ok(0) if filled == 0 => return Ok(false),
      Ok(0) => return Err(ReadError::Truncated),
      Ok(count) => filled += count,
      Err(e) if is_timeout(&e) => {
        if stop() {
          return Err(ReadError::Stopped);
        }
        if serial && (in_record || filled > 0) {
          return Err(ReadError::Idle);
        }
      }
      Err(e) => return Err(ReadError::Io(e)),
    }
  }
  Ok(true)
}

// The next record, or None at the end of the source.
fn read_record(
  source: &mut dyn Read,
  serial: bool,
  stop: &dyn Fn() -> bool,
) -> Result<Option<proto::StorageRecord>, ReadError> {
  let mut size = [0; 8];
  if !read_full(source, &mut size, serial, false, stop)? {
    return Ok(None);
  }
  let size = u64::from_be_bytes(size);
  if size > MAX_RECORD_SIZE {
    return Err(ReadError::BadFrame(format!("invalid record size {size}")));
  }
  let mut bytes = vec![0; size as usize];
  if !bytes.is_empty() && !read_full(source, &mut bytes, serial, true, stop)? {
    return Err(ReadError::Truncated);
  }
  proto::StorageRecord::decode(bytes.as_slice())
    .map(Some)
    .map_err(|e| ReadError::BadFrame(format!("invalid record: {e}")))
}

// Discards input until the line is idle, which is between two records.
fn wait_for_idle(source: &mut dyn Read, stop: &dyn Fn() -> bool) {
  let mut buffer = [0; 256];
  while !stop() {
    match source.read(&mut buffer) {
      Ok(count) if count > 0 => {}
      _ => return,
    }
  }
}

type RecordResult = Result<proto::StorageRecord, String>;

fn forward_records(source: &mut dyn Read, serial: bool, records: &mpsc::Sender<RecordResult>) {
  let stop = || records.is_closed();
  loop {
    let error = match read_record(source, serial, &stop) {
      Ok(Some(record)) => {
        if records.blocking_send(Ok(record)).is_err() {
          return;
        }
        continue;
      }
      Ok(None) | Err(ReadError::Stopped) => return,
      Err(ReadError::Idle) => continue,
      Err(e) => e,
    };
    if !serial || !matches!(error, ReadError::BadFrame(_)) {
      let _ = records.blocking_send(Err(error.to_string()));
      return;
    }
    wait_for_idle(source, &stop);
  }
}

// Runs on its own thread until the source ends or `records` is closed.
fn read_records(path: &str, settings: &SerialSettings, records: &mpsc::Sender<RecordResult>) {
  if is_serial_port(path) {
    match open_serial(path, settings) {
      Ok(mut port) => forward_records(&mut port, true, records),
      Err(e) => {
        let _ = records.blocking_send(Err(format!("failed to open serial port {path}: {e}")));
      }
    }
  } else {
    match File::open(path) {
      Ok(mut file) => forward_records(&mut file, false, records),
      Err(e) => {
        let _ = records.blocking_send(Err(format!("failed to open {path}: {e}")));
      }
    }
  }
}

// When records should be published.
#[derive(Default)]
struct Pacer {
  // The first non zero time and when that record was published.
  first: Option<(u64, Instant)>,
}

impl Pacer {
  fn deadline(&mut self, time: u64, now: Instant) -> Instant {
    if time == 0 {
      return now;
    }
    let (first_time, first_published) = *self.first.get_or_insert((time, now));
    first_published + Duration::from_nanos(time.saturating_sub(first_time))
  }
}

fn analog_encoding(encoding: proto::analog_response::Encoding) -> AnalogEncoding {
  use proto::analog_response::Encoding;
  match encoding {
    Encoding::None => AnalogEncoding::None,
    Encoding::Aac => AnalogEncoding::AAC,
    Encoding::Opus => AnalogEncoding::Opus,
    Encoding::Flac => AnalogEncoding::FLAC,
  }
}

// Messages from before Span.format say Double for every span and use
// is_int_data or is_ulong_data instead.
fn channel_format(body: &proto::AnalogResponse, span: &proto::Span) -> AnalogFormat {
  use proto::span::Format;
  match span.format() {
    Format::Short => AnalogFormat::Short,
    Format::Int => AnalogFormat::Int,
    Format::ULong => AnalogFormat::ULong,
    Format::Encoded => AnalogFormat::Encoded,
    Format::Double if body.is_int_data => AnalogFormat::Int,
    Format::Double if body.is_ulong_data => AnalogFormat::ULong,
    Format::Double => AnalogFormat::Double,
  }
}

// What channels_changed compares between messages.
type ChannelLayout = Vec<(String, AnalogFormat, u64)>;

struct RecordAnalog {
  body: proto::AnalogResponse,
  // int_data narrowed, for Short channels to borrow from.
  short_data: Vec<i16>,
  formats: Vec<AnalogFormat>,
  channels_changed: bool,
  remote_time: Duration,
}

impl RecordAnalog {
  fn new(body: proto::AnalogResponse, remote_time: Duration, layout: &mut Option<ChannelLayout>) -> RecordAnalog {
    let formats: Vec<_> = body.spans.iter().map(|span| channel_format(&body, span)).collect();
    let new_layout: ChannelLayout = body
      .spans
      .iter()
      .enumerate()
      .map(|(channel, span)| {
        (span.name.clone(), formats[channel], body.sample_intervals.get(channel).copied().unwrap_or(0))
      })
      .collect();
    let channels_changed = layout.as_ref() != Some(&new_layout);
    *layout = Some(new_layout);
    let short_data = body.int_data.iter().map(|&value| value as i16).collect();
    RecordAnalog { body, short_data, formats, channels_changed, remote_time }
  }

  fn range(&self, channel: i32) -> Range<usize> {
    let span = &self.body.spans[channel as usize];
    span.begin as usize..span.end as usize
  }

  fn all_channels(&self, format: AnalogFormat) -> bool {
    !self.formats.is_empty() && self.formats.iter().all(|&channel| channel == format)
  }
}

impl AnalogData for RecordAnalog {
  // Spans come from the file, so a bad range reads as an empty channel.
  fn data(&self, channel: i32) -> &[f64] {
    self.body.data.get(self.range(channel)).unwrap_or(&[])
  }

  fn short_data(&self, channel: i32) -> &[i16] {
    self.short_data.get(self.range(channel)).unwrap_or(&[])
  }

  fn int_data(&self, channel: i32) -> &[i32] {
    self.body.int_data.get(self.range(channel)).unwrap_or(&[])
  }

  fn ulong_data(&self, channel: i32) -> &[u64] {
    self.body.ulong_data.get(self.range(channel)).unwrap_or(&[])
  }

  fn num_channels(&self) -> i32 {
    self.body.spans.len() as i32
  }

  fn sample_interval(&self, channel: i32) -> Duration {
    Duration::from_nanos(self.body.sample_intervals.get(channel as usize).copied().unwrap_or(0))
  }

  fn name(&self, channel: i32) -> &str {
    &self.body.spans[channel as usize].name
  }

  fn is_short_data(&self) -> bool {
    self.all_channels(AnalogFormat::Short)
  }

  fn is_int_data(&self) -> bool {
    self.all_channels(AnalogFormat::Int)
  }

  fn is_ulong_data(&self) -> bool {
    self.all_channels(AnalogFormat::ULong)
  }

  fn is_transformed(&self) -> bool {
    self.body.is_transformed
  }

  fn buffer(&self) -> &[u8] {
    &self.body.buffer
  }

  fn encoding(&self) -> AnalogEncoding {
    analog_encoding(self.body.encoding())
  }

  fn analog_format(&self, channel: i32) -> AnalogFormat {
    self.formats[channel as usize]
  }

  fn encoded_count(&self) -> u64 {
    self.body.encoded_count
  }

  fn remote_time(&self) -> Duration {
    self.remote_time
  }

  fn channels_changed(&self) -> bool {
    self.channels_changed
  }

  fn scale(&self, channel: i32) -> f64 {
    if self.body.is_transformed { self.body.spans[channel as usize].scale } else { 1.0 }
  }

  fn offset(&self, channel: i32) -> f64 {
    if self.body.is_transformed { self.body.spans[channel as usize].offset } else { 0.0 }
  }
}

fn image_format(format: i32) -> Option<ImageFormat> {
  use proto::image::Format;
  Some(match Format::try_from(format).ok()? {
    Format::Gray => ImageFormat::Gray,
    Format::Rgb => ImageFormat::RGB,
    Format::Yuyv422 => ImageFormat::YUYV422,
    Format::Yuv420p => ImageFormat::YUV420P,
    Format::Yuvj420p => ImageFormat::YUVJ420P,
    Format::Gray16 => ImageFormat::Gray16,
    Format::Rgb16 => ImageFormat::RGB16,
    Format::Mpeg1 => ImageFormat::MPEG1,
    Format::Mpeg4 => ImageFormat::MPEG4,
    Format::Nv12 => ImageFormat::NV12,
    Format::Bgr => ImageFormat::BGR,
    Format::Mjpeg => ImageFormat::MJPEG,
    Format::H264 => ImageFormat::H264,
    Format::Vp9 => ImageFormat::VP9,
  })
}

struct RecordImage {
  body: proto::Image,
  format: ImageFormat,
  remote_time: Duration,
}

impl ImageData for RecordImage {
  fn plane(&self, channel: i32) -> &[u8] {
    self.body.data.get(channel as usize).map_or(&[], Vec::as_slice)
  }

  fn num_planes(&self) -> u64 {
    self.body.data.len() as u64
  }

  fn format(&self) -> ImageFormat {
    self.format
  }

  fn width(&self) -> u64 {
    u64::from(self.body.width)
  }

  fn height(&self) -> u64 {
    u64::from(self.body.height)
  }

  fn frame_interval(&self) -> Duration {
    Duration::from_nanos(self.body.frame_interval)
  }

  fn remote_time(&self) -> Duration {
    self.remote_time
  }
}

struct RecordMocap {
  segments: Vec<ThalamusMocapSegment>,
  pose_name: String,
  remote_time: Duration,
}

impl RecordMocap {
  fn new(body: proto::XsensResponse, remote_time: Duration) -> RecordMocap {
    let segments = body
      .segments
      .iter()
      .map(|segment| ThalamusMocapSegment {
        frame: segment.frame,
        segment_id: segment.id,
        time: segment.time,
        position: [segment.x, segment.y, segment.z],
        rotation: [segment.q0, segment.q1, segment.q2, segment.q3],
        actor: segment.actor as u8,
      })
      .collect();
    RecordMocap { segments, pose_name: body.pose_name, remote_time }
  }
}

impl MocapData for RecordMocap {
  fn segments(&self) -> &[ThalamusMocapSegment] {
    &self.segments
  }

  fn pose_name(&self) -> &str {
    &self.pose_name
  }

  fn remote_time(&self) -> Duration {
    self.remote_time
  }
}

struct RecordText {
  text: String,
  remote_time: Duration,
}

impl TextData for RecordText {
  fn text(&self) -> &str {
    &self.text
  }

  fn remote_time(&self) -> Duration {
    self.remote_time
  }
}

enum RecordPayload {
  Analog(RecordAnalog),
  Image(RecordImage),
  Mocap(RecordMocap),
  Text(RecordText),
}

struct RecordData {
  payload: RecordPayload,
  time: Duration,
}

impl NodeData for RecordData {
  fn time(&self) -> Duration {
    self.time
  }

  fn analog(&self) -> Option<&dyn AnalogData> {
    match &self.payload {
      RecordPayload::Analog(analog) => Some(analog),
      _ => None,
    }
  }

  fn image(&self) -> Option<&dyn ImageData> {
    match &self.payload {
      RecordPayload::Image(image) => Some(image),
      _ => None,
    }
  }

  fn mocap(&self) -> Option<&dyn MocapData> {
    match &self.payload {
      RecordPayload::Mocap(mocap) => Some(mocap),
      _ => None,
    }
  }

  fn text(&self) -> Option<&dyn TextData> {
    match &self.payload {
      RecordPayload::Text(text) => Some(text),
      _ => None,
    }
  }
}

// The time a message's first producer gave it, which is the record's time for
// data recorded where it was produced.
fn remote_time(body_remote_time: u64, record_time: u64) -> Duration {
  Duration::from_nanos(if body_remote_time != 0 { body_remote_time } else { record_time })
}

// The record's payload, or a description of why it can't be published.
// Thalamus's plugin API can't publish the rest: events and metadata aren't
// node data, and plugin nodes have no blob modality.
fn record_payload(
  record: proto::StorageRecord,
  layout: &mut Option<ChannelLayout>,
) -> Result<RecordPayload, &'static str> {
  let time = record.time;
  match record.body {
    Some(Body::Analog(body)) => {
      let remote_time = remote_time(body.remote_time, time);
      Ok(RecordPayload::Analog(RecordAnalog::new(body, remote_time, layout)))
    }
    Some(Body::Image(body)) => {
      let format = image_format(body.format).ok_or("image with an unknown format")?;
      let remote_time = remote_time(body.remote_time, time);
      Ok(RecordPayload::Image(RecordImage { body, format, remote_time }))
    }
    Some(Body::Xsens(body)) => {
      let remote_time = remote_time(body.remote_time, time);
      Ok(RecordPayload::Mocap(RecordMocap::new(body, remote_time)))
    }
    Some(Body::Text(body)) => {
      Ok(RecordPayload::Text(RecordText { remote_time: remote_time(body.remote_time, time), text: body.text }))
    }
    Some(Body::Event(_)) => Err("event"),
    Some(Body::Compressed(_)) => Err("compressed"),
    Some(Body::Metadata(_)) => Err("metadata"),
    Some(Body::File(_)) => Err("file"),
    Some(Body::Blob(_)) => Err("blob"),
    None => Err("empty"),
  }
}

fn report(api: &ThalamusAPIThreadSafe, text: &str) {
  let text = format!("FILE: {text}");
  println!("{text}");
  api.log(&text);
}

async fn cancellable<T>(f: impl Future<Output = T>, cancel_token: &CancellationToken) -> Result<T, String> {
  tokio::select! {
    biased;
    _ = cancel_token.cancelled() => Err("Cancelled".to_string()),
    result = f => Ok(result),
  }
}

async fn publish_records(
  api: &ThalamusAPIThreadSafe,
  signaler: &OffMainSignaler,
  records: &mut mpsc::Receiver<RecordResult>,
  cancel_token: &CancellationToken,
) -> Result<(), String> {
  let mut pacer = Pacer::default();
  let mut layout = None;
  let mut skipped = HashSet::new();
  loop {
    let record = match cancellable(records.recv(), cancel_token).await {
      Ok(Some(record)) => record?,
      Ok(None) | Err(_) => return Ok(()),
    };
    let deadline = pacer.deadline(record.time, Instant::now());
    if cancellable(tokio::time::sleep_until(deadline.into()), cancel_token).await.is_err() {
      return Ok(());
    }
    let payload = match record_payload(record, &mut layout) {
      Ok(payload) => payload,
      Err(kind) => {
        if skipped.insert(kind) {
          report(api, &format!("skipping {kind} records"));
        }
        continue;
      }
    };
    if signaler.ready(&RecordData { payload, time: api.time() }).is_err() {
      return Ok(());
    }
  }
}

async fn run(
  api: ThalamusAPIThreadSafe,
  signaler: &OffMainSignaler,
  path: String,
  settings: SerialSettings,
  cancel_token: &CancellationToken,
) -> Result<(), String> {
  if path.is_empty() {
    return Err("File isn't set".to_string());
  }
  let (records_tx, mut records_rx) = mpsc::channel(READ_AHEAD);
  let (reader_done_tx, reader_done_rx) = oneshot::channel::<()>();
  // A thread, not spawn_blocking: a read that never returns mustn't hold up
  // the runtime's shutdown.
  std::thread::spawn(move || {
    read_records(&path, &settings, &records_tx);
    drop(reader_done_tx);
  });

  let result = publish_records(&api, signaler, &mut records_rx, cancel_token).await;

  // The next run may need the serial port this one has open.
  drop(records_rx);
  let _ = tokio::time::timeout(READER_EXIT_TIMEOUT, reader_done_rx).await;
  result
}

pub struct FileNode {
  api: ThalamusAPI,
  main_thread_token: MainThreadToken,
  state: State,
  _state_connection: Option<OnDrop>,
  signaler: Arc<OffMainSignaler>,
  // The last run's task. It may still be shutting down after stop(); the
  // next run waits for it so two runs never read the same port.
  task: Option<JoinHandle<()>>,
  cancel_token: Option<CancellationToken>,
}

// Cancels the current run and blocks the signaler, then calls on_stopped on
// the main thread once no ready() is in progress.
fn stop<F: FnOnce() + 'static>(inner: &Rc<RefCell<FileNode>>, on_stopped: F) {
  let borrow = inner.borrow();
  if let Some(cancel) = &borrow.cancel_token {
    cancel.cancel();
  }
  borrow.signaler.block_then(borrow.main_thread_token, on_stopped);
}

fn start(inner: &Rc<RefCell<FileNode>>) {
  let mut borrow = inner.borrow_mut();
  let api = borrow.api.thread_safe();
  let signaler = borrow.signaler.clone();
  let path = match borrow.state.get(StateKey::String("File".to_string())) {
    Some(StateValue::String(path)) => path,
    _ => String::new(),
  };
  let settings = serial_settings(&borrow.state);
  let previous = borrow.task.take();
  let wrapped_state = MainThreadOnly::new(borrow.state.clone(), borrow.api, borrow.main_thread_token);
  let cancel_token = CancellationToken::new();
  // Two queued starts: end the first run, which the new one then waits for.
  if let Some(old) = borrow.cancel_token.replace(cancel_token.clone()) {
    old.cancel();
  }
  borrow.signaler.unblock();
  borrow.task = Some(api.tokio().as_ref().unwrap().spawn(async move {
    if let Some(previous) = previous {
      let _ = previous.await;
    }
    let result = match settings {
      Ok(settings) => run(api, &signaler, path, settings, &cancel_token).await,
      Err(e) => Err(e),
    };
    if let Err(e) = result {
      report(&api, &e);
    }
    if !cancel_token.is_cancelled() {
      api.post_to_main(|main_thread_token| {
        let state = wrapped_state.take(main_thread_token);
        state.set(StateKey::String("Running".to_string()), StateValue::Bool(false));
      });
    }
  }));
}

fn on_state(inner: &Rc<RefCell<FileNode>>, key: StateKey) {
  let StateKey::String(key_str) = key else {
    return;
  };
  if key_str != "Running" {
    return;
  }
  let weak = Rc::downgrade(inner);
  // on_stopped runs later, so check Running then rather than now: it may have
  // been turned off again in the meantime.
  stop(inner, move || {
    let Some(inner) = weak.upgrade() else {
      return;
    };
    let running = inner.borrow().state.get(StateKey::String("Running".to_string()));
    if running == Some(StateValue::Bool(true)) {
      start(&inner);
    }
  });
}

impl NodeConsts for FileNode {
  const MODALITIES: u32 =
    THALAMUS_MODALITY_ANALOG | THALAMUS_MODALITY_IMAGE | THALAMUS_MODALITY_MOCAP | THALAMUS_MODALITY_TEXT;
  const SIGNALS_OFFMAIN: bool = true;
}

impl Node for FileNode {
  fn new(
    api: ThalamusAPI,
    node_token: NodeToken,
    state: State,
    main_thread_token: MainThreadToken,
  ) -> Rc<RefCell<Self>> {
    let inner = Rc::new(RefCell::new(FileNode {
      api,
      main_thread_token,
      state: state.clone(),
      _state_connection: None,
      // Created blocked; start() unblocks it.
      signaler: OffMainSignaler::new(api, node_token.clone()),
      task: None,
      cancel_token: None,
    }));

    let callback_ref: Weak<RefCell<FileNode>> = Rc::downgrade(&inner);
    let connection = state.connect(
      move |source: State, _action: StateAction, key: StateKey, _value: StateValue| {
        if let Some(inner) = callback_ref.upgrade() {
          if source == inner.borrow().state {
            on_state(&inner, key);
          }
        }
      },
    );
    inner.borrow_mut()._state_connection = Some(connection);

    let predrop_ref = Rc::downgrade(&inner);
    node_token.set_predrop(move |token| {
      let Some(inner) = predrop_ref.upgrade() else {
        return token.ready();
      };
      let borrow = inner.borrow();
      if let Some(cancel) = &borrow.cancel_token {
        cancel.cancel();
      }
      // The task can outlive the node: once the node is destroyed its
      // ready() fails and it exits.
      borrow.signaler.predrop(token);
    });

    state.recap();

    inner
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::storage_record::frame;
  use std::collections::VecDeque;

  fn encoded(record: &proto::StorageRecord) -> Vec<u8> {
    let mut output = Vec::new();
    frame(&mut output, &record.encode_to_vec());
    output
  }

  fn text_record(text: &str, time: u64) -> proto::StorageRecord {
    proto::StorageRecord {
      body: Some(Body::Text(proto::Text { text: text.to_string(), ..Default::default() })),
      time,
      ..Default::default()
    }
  }

  fn never() -> bool {
    false
  }

  #[test]
  fn reads_records_until_the_end_of_the_source() {
    let mut bytes = encoded(&text_record("a", 1));
    bytes.extend(encoded(&text_record("b", 2)));
    let mut source = bytes.as_slice();
    assert_eq!(read_record(&mut source, false, &never).unwrap(), Some(text_record("a", 1)));
    assert_eq!(read_record(&mut source, false, &never).unwrap(), Some(text_record("b", 2)));
    assert_eq!(read_record(&mut source, false, &never).unwrap(), None);
  }

  #[test]
  fn rejects_truncated_records() {
    let bytes = encoded(&text_record("hello", 1));
    for len in [3, 8, bytes.len() - 1] {
      let mut source = &bytes[..len];
      assert!(matches!(read_record(&mut source, false, &never), Err(ReadError::Truncated)), "{len}");
    }
  }

  #[test]
  fn rejects_corrupt_frames() {
    let mut source: &[u8] = &[0xff; 8];
    assert!(matches!(read_record(&mut source, false, &never), Err(ReadError::BadFrame(_))));
    let mut source: &[u8] = &[0, 0, 0, 0, 0, 0, 0, 2, 0xff, 0xff];
    assert!(matches!(read_record(&mut source, false, &never), Err(ReadError::BadFrame(_))));
  }

  // Returns each chunk from one read, with a timeout between them.
  struct SerialLike(VecDeque<Vec<u8>>);

  impl Read for SerialLike {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
      match self.0.pop_front() {
        Some(mut chunk) if !chunk.is_empty() => {
          let count = chunk.len().min(buffer.len());
          buffer[..count].copy_from_slice(&chunk[..count]);
          if count < chunk.len() {
            self.0.push_front(chunk.split_off(count));
          }
          Ok(count)
        }
        Some(_) => Err(io::Error::new(ErrorKind::TimedOut, "timed out")),
        None => Ok(0),
      }
    }
  }

  #[test]
  fn timeouts_between_records_keep_reading() {
    let mut source = SerialLike(VecDeque::from([vec![], vec![], encoded(&text_record("hello", 1))]));
    assert_eq!(read_record(&mut source, true, &never).unwrap(), Some(text_record("hello", 1)));
  }

  #[test]
  fn timeouts_inside_a_serial_record_are_idle_errors() {
    let bytes = encoded(&text_record("hello", 1));
    for split in [4, 8, 10] {
      let (head, tail) = bytes.split_at(split);
      let mut source = SerialLike(VecDeque::from([head.to_vec(), vec![], tail.to_vec()]));
      assert!(matches!(read_record(&mut source, true, &never), Err(ReadError::Idle)), "{split}");
    }
  }

  #[test]
  fn timeouts_stop_when_asked() {
    let mut source = SerialLike(VecDeque::from([vec![]]));
    assert!(matches!(read_record(&mut source, true, &|| true), Err(ReadError::Stopped)));
  }

  fn forward(source: &mut dyn Read, serial: bool) -> Vec<RecordResult> {
    let (tx, mut rx) = mpsc::channel(READ_AHEAD);
    forward_records(source, serial, &tx);
    drop(tx);
    let mut results = Vec::new();
    while let Ok(result) = rx.try_recv() {
      results.push(result);
    }
    results
  }

  #[test]
  fn serial_streams_resynchronize_when_joined_inside_a_record() {
    // Every offset: some give a plausible size, others a bad size or record.
    let first = encoded(&text_record("a", 0));
    for offset in 1..first.len() {
      let mut source =
        SerialLike(VecDeque::from([first[offset..].to_vec(), vec![], vec![], encoded(&text_record("b", 0))]));
      let results = forward(&mut source, true);
      assert_eq!(results, [Ok(text_record("b", 0))], "{offset}");
    }
  }

  #[test]
  fn files_fail_on_corrupt_frames() {
    let mut bytes = encoded(&text_record("a", 0));
    bytes.extend([0xff; 8]);
    let results = forward(&mut bytes.as_slice(), false);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0], Ok(text_record("a", 0)));
    assert!(results[1].is_err());
  }

  #[test]
  fn zero_times_are_published_immediately() {
    let mut pacer = Pacer::default();
    let start = Instant::now();
    assert_eq!(pacer.deadline(0, start), start);
    let later = start + Duration::from_secs(5);
    assert_eq!(pacer.deadline(0, later), later);
  }

  #[test]
  fn times_are_relative_to_the_first_record() {
    let mut pacer = Pacer::default();
    let start = Instant::now();
    let second = 1_000_000_000;
    assert_eq!(pacer.deadline(5 * second, start), start);
    let read = start + Duration::from_millis(10);
    assert_eq!(pacer.deadline(6 * second, read), start + Duration::from_secs(1));
    assert_eq!(pacer.deadline(8 * second, read), start + Duration::from_secs(3));
    // Zero time records don't disturb the pacing.
    assert_eq!(pacer.deadline(0, read), read);
    assert_eq!(pacer.deadline(9 * second, read), start + Duration::from_secs(4));
  }

  #[test]
  fn times_before_the_first_are_published_at_once() {
    let mut pacer = Pacer::default();
    let start = Instant::now();
    pacer.deadline(100, start);
    assert_eq!(pacer.deadline(50, start), start);
  }

  fn span(name: &str, range: Range<u32>, format: proto::span::Format) -> proto::Span {
    proto::Span { begin: range.start, end: range.end, name: name.to_string(), format: format as i32, ..Default::default() }
  }

  fn record_analog(body: proto::AnalogResponse) -> RecordAnalog {
    RecordAnalog::new(body, Duration::ZERO, &mut None)
  }

  #[test]
  fn analog_channels_keep_their_formats() {
    use proto::span::Format;
    let analog = record_analog(proto::AnalogResponse {
      data: vec![1.5],
      int_data: vec![-3, 70000],
      ulong_data: vec![9],
      spans: vec![
        span("d", 0..1, Format::Double),
        span("s", 0..1, Format::Short),
        span("i", 1..2, Format::Int),
        span("u", 0..1, Format::ULong),
      ],
      sample_intervals: vec![1, 2, 3, 4],
      ..Default::default()
    });
    assert_eq!(analog.data(0), [1.5]);
    assert_eq!(analog.short_data(1), [-3]);
    assert_eq!(analog.int_data(2), [70000]);
    assert_eq!(analog.ulong_data(3), [9]);
    assert_eq!(analog.count(1), 1);
    assert_eq!(analog.sample_interval(3), Duration::from_nanos(4));
    assert!(!analog.is_short_data() && !analog.is_int_data() && !analog.is_ulong_data());
  }

  #[test]
  fn uniform_short_channels_are_short_data() {
    use proto::span::Format;
    let analog = record_analog(proto::AnalogResponse {
      int_data: vec![1, 2],
      is_int_data: true,
      spans: vec![span("a", 0..1, Format::Short), span("b", 1..2, Format::Short)],
      ..Default::default()
    });
    assert!(analog.is_short_data() && !analog.is_int_data());
  }

  #[test]
  fn old_records_use_the_message_wide_flags() {
    use proto::span::Format;
    let analog = record_analog(proto::AnalogResponse {
      int_data: vec![1],
      is_int_data: true,
      spans: vec![span("a", 0..1, Format::Double)],
      ..Default::default()
    });
    assert!(analog.is_int_data());
    assert_eq!(analog.int_data(0), [1]);
  }

  #[test]
  fn bad_spans_read_as_empty_channels() {
    use proto::span::Format;
    let analog = record_analog(proto::AnalogResponse {
      data: vec![1.0],
      spans: vec![span("a", 0..5, Format::Double)],
      ..Default::default()
    });
    assert!(analog.data(0).is_empty());
    assert_eq!(analog.sample_interval(0), Duration::ZERO);
  }

  #[test]
  fn channels_changed_tracks_the_layout() {
    use proto::span::Format;
    let body = |name: &str, interval| proto::AnalogResponse {
      spans: vec![span(name, 0..0, Format::Double)],
      sample_intervals: vec![interval],
      ..Default::default()
    };
    let mut layout = None;
    let mut changed = |body| RecordAnalog::new(body, Duration::ZERO, &mut layout).channels_changed();
    assert!(changed(body("a", 1)));
    assert!(!changed(body("a", 1)));
    assert!(changed(body("b", 1)));
    assert!(changed(body("b", 2)));
    assert!(!changed(body("b", 2)));
  }

  #[test]
  fn analog_scale_only_applies_to_transformed_data() {
    use proto::span::Format;
    let spans = vec![proto::Span { scale: 0.5, offset: 2.0, ..span("a", 0..0, Format::Double) }];
    let analog = record_analog(proto::AnalogResponse { spans: spans.clone(), ..Default::default() });
    assert_eq!((analog.scale(0), analog.offset(0)), (1.0, 0.0));
    let analog = record_analog(proto::AnalogResponse { spans, is_transformed: true, ..Default::default() });
    assert_eq!((analog.scale(0), analog.offset(0)), (0.5, 2.0));
  }

  #[test]
  fn records_round_trip_through_storage_record_encoders() {
    struct Text;
    impl TextData for Text {
      fn text(&self) -> &str {
        "hello"
      }
    }
    let bytes = crate::storage_record::text("node", 7, &Text);
    let record = proto::StorageRecord::decode(bytes.as_slice()).unwrap();
    let data = RecordData { payload: record_payload(record, &mut None).ok().unwrap(), time: Duration::ZERO };
    let text = data.text().unwrap();
    assert_eq!(text.text(), "hello");
    // The recorded time is kept as the time on the producer.
    assert_eq!(text.remote_time(), Duration::from_nanos(7));
    assert!(data.analog().is_none() && data.image().is_none() && data.mocap().is_none());
  }

  #[test]
  fn image_records_map_formats_and_planes() {
    let body = proto::Image {
      data: vec![vec![1, 2], vec![3]],
      width: 2,
      height: 1,
      format: proto::image::Format::Mpeg1 as i32,
      remote_time: 3,
      ..Default::default()
    };
    let record = proto::StorageRecord { body: Some(Body::Image(body)), time: 9, ..Default::default() };
    let RecordPayload::Image(image) = record_payload(record, &mut None).ok().unwrap() else {
      panic!("not an image");
    };
    assert_eq!(image.format(), ImageFormat::MPEG1);
    assert_eq!((image.num_planes(), image.plane(1), image.plane(2)), (2, &[3u8][..], &[][..]));
    assert_eq!(ImageData::remote_time(&image), Duration::from_nanos(3));
  }

  #[test]
  fn mocap_records_map_segments() {
    let segment = proto::XsensSegment { id: 4, x: 1.0, q3: 2.0, frame: 5, time: 6, actor: 7, ..Default::default() };
    let body = proto::XsensResponse { segments: vec![segment], pose_name: "pose".to_string(), ..Default::default() };
    let record = proto::StorageRecord { body: Some(Body::Xsens(body)), ..Default::default() };
    let RecordPayload::Mocap(mocap) = record_payload(record, &mut None).ok().unwrap() else {
      panic!("not mocap");
    };
    let segment = &mocap.segments()[0];
    assert_eq!((segment.segment_id, segment.frame, segment.time, segment.actor), (4, 5, 6, 7));
    assert_eq!((segment.position[0], segment.rotation[3]), (1.0, 2.0));
    assert_eq!(mocap.pose_name(), "pose");
  }

  #[test]
  fn unpublishable_records_are_skipped() {
    let event = proto::StorageRecord { body: Some(Body::Event(Default::default())), ..Default::default() };
    assert_eq!(record_payload(event, &mut None).err(), Some("event"));
    assert_eq!(record_payload(Default::default(), &mut None).err(), Some("empty"));
    let image = proto::Image { format: 1000, ..Default::default() };
    let record = proto::StorageRecord { body: Some(Body::Image(image)), ..Default::default() };
    assert!(record_payload(record, &mut None).is_err());
  }

  #[cfg(windows)]
  #[test]
  fn recognizes_com_ports() {
    for path in ["COM1", "com12", r"\\.\COM3"] {
      assert!(is_serial_port(path), "{path}");
    }
    for path in ["COM", "COMPUTER", "C:/data/COM3", "rec.tha", "COMé"] {
      assert!(!is_serial_port(path), "{path}");
    }
  }

  #[cfg(unix)]
  #[test]
  fn regular_files_and_non_terminals_arent_serial_ports() {
    assert!(!is_serial_port("/dev/null"));
    assert!(!is_serial_port("/does/not/exist"));
  }
}
