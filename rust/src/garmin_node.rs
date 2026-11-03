// Receives motion and heart rate data from the Connect IQ app in
// garmin/. Connect IQ apps can only be BLE centrals, so this node is the
// peripheral: it advertises GARMIN_SERVICE_UUID and the watch writes packets
// (see parse_packet) to GARMIN_DATA_CHAR_UUID. btleplug is central-only, so
// this uses ble-peripheral-rust. If "Serial Port" is set the node reads the
// same packets from that port instead, forwarded by arduino/thalamus_bridge in
// the frames Deframer reads.
use crate::api::{
  AnalogData, MainThreadOnly, MainThreadToken, Node, NodeConsts, NodeData, NodeToken,
  OffMainSignaler, OnDrop, State, StateAction, StateKey, StateValue, THALAMUS_MODALITY_ANALOG,
  ThalamusAPI, ThalamusAPIThreadSafe,
};
use ble_peripheral_rust::gatt::characteristic::Characteristic;
use ble_peripheral_rust::gatt::peripheral_event::{
  PeripheralEvent, RequestResponse, WriteRequestResponse,
};
use ble_peripheral_rust::gatt::properties::{AttributePermission, CharacteristicProperty};
use ble_peripheral_rust::gatt::service::Service;
use ble_peripheral_rust::{Peripheral, PeripheralImpl};
use crate::serial_config::{SerialSettings, open_serial, serial_settings};
use std::cell::RefCell;
use std::io::{ErrorKind, Read};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::{Uuid, uuid};

// Must match garmin/source/ThalamusProfile.mc.
pub const GARMIN_SERVICE_UUID: Uuid = uuid!("7a4b0001-5c2e-4f6a-9d3b-8e1f2a6c4d90");
pub const GARMIN_DATA_CHAR_UUID: Uuid = uuid!("7a4b0002-5c2e-4f6a-9d3b-8e1f2a6c4d90");

const PACKET_HEART_RATE: u8 = 1;
const PACKET_ACCEL: u8 = 2;
const PACKET_GYRO: u8 = 3;
const PACKET_MAG: u8 = 4;
// Only from arduino/thalamus_bridge, not the watch: a status message from the
// bridge. It isn't part of the watch's sequence numbers.
const PACKET_TEXT: u8 = 0x7f;
const HEADER_SIZE: usize = 2;
const VECTOR_SAMPLE_SIZE: usize = 6;

// The watch samples motion at this rate (MOTION_RATE_HZ in
// garmin/source/SensorSource.mc).
const MOTION_INTERVAL: Duration = Duration::from_millis(40);
// Heart rate is sent once a second.
const HEART_RATE_INTERVAL: Duration = Duration::from_secs(1);

const CHANNEL_COUNT: usize = 10;
const CHANNEL_NAMES: [&str; CHANNEL_COUNT] =
  ["HR", "ACC_X", "ACC_Y", "ACC_Z", "GYR_X", "GYR_Y", "GYR_Z", "MAG_X", "MAG_Y", "MAG_Z"];
const HR_CHANNEL: usize = 0;
const ACC_CHANNEL: usize = 1;
const GYR_CHANNEL: usize = 4;
const MAG_CHANNEL: usize = 7;

const ACC_SCALE: f64 = 0.001;
// Must match GYRO_SCALE in garmin/source/SensorSource.mc.
const GYRO_SCALE: f64 = 0.1;

#[derive(Debug, PartialEq)]
pub enum Payload {
  // Beats per minute, 0 if the watch has no reading.
  HeartRate(u8),
  // x, y, z samples, oldest first, in milli-g.
  Accel(Vec<[i16; 3]>),
  // In 0.1 deg/s.
  Gyro(Vec<[i16; 3]>),
  // In milli-gauss.
  Mag(Vec<[i16; 3]>),
  Text(String),
}

#[derive(Debug, PartialEq)]
pub struct Packet {
  pub seq: u8,
  pub payload: Payload,
}

// Packets are at most 20 bytes, the ATT payload at the default MTU, all
// little endian:
//   byte 0: type (1 = heart rate, 2 = accelerometer, 3 = gyroscope,
//           4 = magnetometer)
//   byte 1: sequence number, incremented per packet of any type, wrapping
//   heart rate: byte 2 = bpm (u8)
//   others: 1 to 3 samples of i16 x, y, z
// Text packets (type 0x7f) can be longer and hold UTF-8 after a zero byte 1.
pub fn parse_packet(bytes: &[u8]) -> Result<Packet, String> {
  if bytes.len() < HEADER_SIZE {
    return Err(format!("packet too short: {} bytes", bytes.len()));
  }
  let seq = bytes[1];
  let body = &bytes[HEADER_SIZE..];
  let payload = match bytes[0] {
    PACKET_HEART_RATE => {
      let [bpm] = body else {
        return Err(format!("heart rate packet has {} payload bytes", body.len()));
      };
      Payload::HeartRate(*bpm)
    }
    PACKET_ACCEL => Payload::Accel(parse_vectors(body)?),
    PACKET_GYRO => Payload::Gyro(parse_vectors(body)?),
    PACKET_MAG => Payload::Mag(parse_vectors(body)?),
    PACKET_TEXT => Payload::Text(String::from_utf8_lossy(body).into_owned()),
    other => return Err(format!("unknown packet type {other}")),
  };
  Ok(Packet { seq, payload })
}

fn parse_vectors(body: &[u8]) -> Result<Vec<[i16; 3]>, String> {
  if body.is_empty() || body.len() % VECTOR_SAMPLE_SIZE != 0 {
    return Err(format!("vector packet has {} payload bytes", body.len()));
  }
  Ok(
    body
      .chunks_exact(VECTOR_SAMPLE_SIZE)
      .map(|s| {
        [
          i16::from_le_bytes([s[0], s[1]]),
          i16::from_le_bytes([s[2], s[3]]),
          i16::from_le_bytes([s[4], s[5]]),
        ]
      })
      .collect(),
  )
}

const FRAME_END: u8 = 0xa5;
// Must match MAX_FRAME_PAYLOAD in arduino/thalamus_bridge/serial_frame.h.
const MAX_FRAME_PAYLOAD: usize = 64;

// Finds packets in a serial stream of frames:
//   byte 0: packet length, 1 to MAX_FRAME_PAYLOAD
//   then the packet, then 0xa5
// If a frame doesn't end in 0xa5 only its first byte is discarded, so it locks
// on from anywhere in a stream. There's no checksum: a corrupted sample inside
// a frame isn't detected.
#[derive(Default)]
struct Deframer {
  buffer: Vec<u8>,
}

impl Deframer {
  fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
    self.buffer.extend_from_slice(bytes);
    let mut packets = Vec::new();
    let mut start = 0;
    while let Some(&length) = self.buffer.get(start) {
      let length = usize::from(length);
      if length == 0 || length > MAX_FRAME_PAYLOAD {
        start += 1;
        continue;
      }
      let end = start + 1 + length;
      let Some(&last) = self.buffer.get(end) else {
        break;
      };
      if last == FRAME_END {
        packets.push(self.buffer[start + 1..end].to_vec());
        start = end + 1;
      } else {
        start += 1;
      }
    }
    self.buffer.drain(..start);
    packets
  }
}

// Packets lost between `expected` and `seq`, counting wraparound.
fn missing_packets(expected: u8, seq: u8) -> u8 {
  seq.wrapping_sub(expected)
}

struct GarminData<'a> {
  channels: &'a [Vec<i16>; CHANNEL_COUNT],
  time: Duration,
}

impl<'a> NodeData for GarminData<'a> {
  fn time(&self) -> Duration {
    self.time
  }
  fn analog(&self) -> Option<&dyn AnalogData> {
    Some(self)
  }
}

impl<'a> AnalogData for GarminData<'a> {
  fn short_data(&self, channel: i32) -> &[i16] {
    &self.channels[channel as usize]
  }

  fn num_channels(&self) -> i32 {
    self.channels.len() as i32
  }

  fn sample_interval(&self, channel: i32) -> Duration {
    if channel as usize == HR_CHANNEL {
      HEART_RATE_INTERVAL
    } else {
      MOTION_INTERVAL
    }
  }

  fn name(&self, channel: i32) -> &str {
    CHANNEL_NAMES[channel as usize]
  }

  fn is_short_data(&self) -> bool {
    true
  }

  fn is_transformed(&self) -> bool {
    true
  }

  // Heart rate in bpm, acceleration in g, rotation in deg/s and the magnetic
  // field in milli-gauss.
  fn scale(&self, channel: i32) -> f64 {
    match channel as usize {
      channel if channel >= MAG_CHANNEL => 1.0,
      channel if channel >= GYR_CHANNEL => GYRO_SCALE,
      channel if channel >= ACC_CHANNEL => ACC_SCALE,
      _ => 1.0,
    }
  }
}

fn report(api: &ThalamusAPIThreadSafe, text: &str) {
  let text = format!("GARMIN: {text}");
  println!("{text}");
  api.log(&text);
}

async fn cancellable<T>(
  f: impl Future<Output = T>,
  cancel_token: &CancellationToken,
) -> Result<T, String> {
  tokio::select! {
    biased;
    _ = cancel_token.cancelled() => Err("Cancelled".to_string()),
    result = f => Ok(result),
  }
}

fn data_service() -> Service {
  Service {
    uuid: GARMIN_SERVICE_UUID,
    primary: true,
    characteristics: vec![Characteristic {
      uuid: GARMIN_DATA_CHAR_UUID,
      properties: vec![
        CharacteristicProperty::Write,
        CharacteristicProperty::WriteWithoutResponse,
      ],
      permissions: vec![AttributePermission::Writeable],
      ..Default::default()
    }],
  }
}

fn push_vectors(channels: &mut [Vec<i16>; CHANNEL_COUNT], first_channel: usize, samples: Vec<[i16; 3]>) {
  for sample in samples {
    for (axis, value) in sample.into_iter().enumerate() {
      channels[first_channel + axis].push(value);
    }
  }
}

// Turns packets into data, from either transport.
#[derive(Default)]
struct PacketPublisher {
  channels: [Vec<i16>; CHANNEL_COUNT],
  expected_seq: Option<u8>,
}

impl PacketPublisher {
  // Returns false once the node is destroyed.
  fn publish(&mut self, api: &ThalamusAPIThreadSafe, signaler: &OffMainSignaler, bytes: &[u8]) -> bool {
    let packet = match parse_packet(bytes) {
      Ok(packet) => packet,
      Err(e) => {
        report(api, &format!("dropping packet: {e}"));
        return true;
      }
    };
    if let Payload::Text(text) = &packet.payload {
      report(api, text);
      return true;
    }
    if let Some(expected) = self.expected_seq {
      let missing = missing_packets(expected, packet.seq);
      if missing != 0 {
        report(api, &format!("missed {missing} packets"));
      }
    }
    self.expected_seq = Some(packet.seq.wrapping_add(1));

    match packet.payload {
      Payload::HeartRate(bpm) => self.channels[HR_CHANNEL].push(i16::from(bpm)),
      Payload::Accel(samples) => push_vectors(&mut self.channels, ACC_CHANNEL, samples),
      Payload::Gyro(samples) => push_vectors(&mut self.channels, GYR_CHANNEL, samples),
      Payload::Mag(samples) => push_vectors(&mut self.channels, MAG_CHANNEL, samples),
      Payload::Text(_) => {}
    }

    let data = GarminData { channels: &self.channels, time: api.time() };
    if signaler.ready(&data).is_err() {
      return false;
    }
    self.channels.iter_mut().for_each(Vec::clear);
    true
  }
}

type PacketResult = Result<Vec<u8>, String>;

// Runs on its own thread until the port fails or `packets` is closed.
fn read_serial(path: &str, settings: &SerialSettings, packets: &mpsc::Sender<PacketResult>) {
  let mut port = match open_serial(path, settings) {
    Ok(port) => port,
    Err(e) => {
      let _ = packets.blocking_send(Err(format!("failed to open serial port {path}: {e}")));
      return;
    }
  };
  let mut deframer = Deframer::default();
  let mut buffer = [0; 256];
  while !packets.is_closed() {
    match port.read(&mut buffer) {
      Ok(count) => {
        for packet in deframer.push(&buffer[..count]) {
          if packets.blocking_send(Ok(packet)).is_err() {
            return;
          }
        }
      }
      Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
      Err(e) => {
        let _ = packets.blocking_send(Err(format!("read failed: {e}")));
        return;
      }
    }
  }
}

async fn serve_serial(
  api: ThalamusAPIThreadSafe,
  signaler: &OffMainSignaler,
  path: String,
  settings: SerialSettings,
  cancel_token: &CancellationToken,
) -> Result<(), String> {
  report(&api, &format!("reading {path}"));
  let (packets_tx, mut packets_rx) = mpsc::channel(256);
  let (reader_done_tx, reader_done_rx) = oneshot::channel::<()>();
  std::thread::spawn(move || {
    read_serial(&path, &settings, &packets_tx);
    drop(reader_done_tx);
  });

  let mut publisher = PacketPublisher::default();
  let result = loop {
    match cancellable(packets_rx.recv(), cancel_token).await {
      Ok(Some(Ok(packet))) => {
        if !publisher.publish(&api, signaler, &packet) {
          break Ok(());
        }
      }
      Ok(Some(Err(e))) => break Err(e),
      Ok(None) | Err(_) => break Ok(()),
    }
  };

  // The next run may need the port. Reads end within SERIAL_TIMEOUT.
  drop(packets_rx);
  let _ = reader_done_rx.await;
  result
}

async fn serve(
  api: ThalamusAPIThreadSafe,
  signaler: &OffMainSignaler,
  name: &str,
  cancel_token: &CancellationToken,
) -> Result<(), String> {
  let (event_tx, mut event_rx) = mpsc::channel::<PeripheralEvent>(256);
  let mut peripheral = cancellable(Peripheral::new(event_tx), cancel_token)
    .await?
    .map_err(|e| format!("failed to open the Bluetooth adapter: {e}"))?;

  while !peripheral
    .is_powered()
    .await
    .map_err(|e| format!("failed to read the adapter state: {e}"))?
  {
    report(&api, "waiting for the Bluetooth adapter to power on");
    cancellable(tokio::time::sleep(Duration::from_secs(1)), cancel_token).await?;
  }

  peripheral
    .add_service(&data_service())
    .await
    .map_err(|e| format!("failed to add the service: {e}"))?;
  // Windows ignores the name; the watch finds the service by its UUID.
  peripheral
    .start_advertising(name, &[GARMIN_SERVICE_UUID])
    .await
    .map_err(|e| format!("failed to start advertising: {e}"))?;
  report(&api, &format!("advertising {GARMIN_SERVICE_UUID} as {name}"));

  let mut publisher = PacketPublisher::default();

  let result = loop {
    let event = match cancellable(event_rx.recv(), cancel_token).await {
      Ok(Some(event)) => event,
      Ok(None) => break Err("the Bluetooth event stream closed".to_string()),
      Err(_) => break Ok(()),
    };
    let PeripheralEvent::WriteRequest { request, value, offset, responder } = event else {
      continue;
    };
    if request.characteristic != GARMIN_DATA_CHAR_UUID {
      let _ = responder.send(WriteRequestResponse { response: RequestResponse::InvalidHandle });
      continue;
    }
    if offset != 0 {
      let _ = responder.send(WriteRequestResponse { response: RequestResponse::InvalidOffset });
      continue;
    }
    // Respond before publishing so the watch can send its next packet.
    let _ = responder.send(WriteRequestResponse { response: RequestResponse::Success });

    if !publisher.publish(&api, signaler, &value) {
      break Ok(());
    }
  };

  let _ = peripheral.stop_advertising().await;
  result
}

pub struct GarminNode {
  api: ThalamusAPI,
  main_thread_token: MainThreadToken,
  state: State,
  _state_connection: Option<OnDrop>,
  signaler: Arc<OffMainSignaler>,
  ble_name: String,
  // The last run's task. It may still be shutting down after stop(); the
  // next run waits for it so two peripherals never serve the same service.
  task: Option<JoinHandle<()>>,
  cancel_token: Option<CancellationToken>,
}

// Cancels the current run and blocks the signaler, then calls on_stopped on
// the main thread once no ready() is in progress. The run's task may still be
// releasing the adapter; start() waits for it.
fn stop<F: FnOnce() + 'static>(inner: &Rc<RefCell<GarminNode>>, on_stopped: F) {
  let borrow = inner.borrow();
  if let Some(cancel) = &borrow.cancel_token {
    cancel.cancel();
  }
  borrow.signaler.block_then(borrow.main_thread_token, on_stopped);
}

fn start(inner: &Rc<RefCell<GarminNode>>) {
  let mut borrow = inner.borrow_mut();
  let api = borrow.api.thread_safe();
  let signaler = borrow.signaler.clone();
  let ble_name = borrow.ble_name.clone();
  let serial_port = match borrow.state.get(StateKey::String("Serial Port".to_string())) {
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
    let result = if serial_port.is_empty() {
      serve(api, &signaler, &ble_name, &cancel_token).await
    } else {
      match settings {
        Ok(settings) => serve_serial(api, &signaler, serial_port, settings, &cancel_token).await,
        Err(e) => Err(e),
      }
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

fn on_state(inner: &Rc<RefCell<GarminNode>>, key: StateKey, value: StateValue) {
  let StateKey::String(key_str) = key else {
    return;
  };
  match key_str.as_str() {
    "Running" => {
      let weak = Rc::downgrade(inner);
      // on_stopped runs later, so check Running then rather than now: it
      // may have been turned off again in the meantime.
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
    "BLE Name" => {
      if let StateValue::String(ble_name) = value {
        inner.borrow_mut().ble_name = ble_name;
      }
    }
    _ => {}
  }
}

impl NodeConsts for GarminNode {
  const MODALITIES: u32 = THALAMUS_MODALITY_ANALOG;
  const SIGNALS_OFFMAIN: bool = true;
}

impl Node for GarminNode {
  fn new(
    api: ThalamusAPI,
    node_token: NodeToken,
    state: State,
    main_thread_token: MainThreadToken,
  ) -> Rc<RefCell<Self>> {
    let inner = Rc::new(RefCell::new(GarminNode {
      api,
      main_thread_token,
      state: state.clone(),
      _state_connection: None,
      // Created blocked; start() unblocks it.
      signaler: OffMainSignaler::new(api, node_token.clone()),
      ble_name: "Thalamus".to_string(),
      task: None,
      cancel_token: None,
    }));

    let callback_ref: Weak<RefCell<GarminNode>> = Rc::downgrade(&inner);
    let connection = state.connect(
      move |source: State, _action: StateAction, key: StateKey, value: StateValue| {
        if let Some(inner) = callback_ref.upgrade() {
          if source == inner.borrow().state {
            on_state(&inner, key, value);
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

  #[test]
  fn parses_heart_rate() {
    assert_eq!(parse_packet(&[1, 7, 72]), Ok(Packet { seq: 7, payload: Payload::HeartRate(72) }));
  }

  #[test]
  fn parses_gyroscope_and_magnetometer() {
    let mut body = Vec::new();
    for v in [1i16, -2, 3] {
      body.extend_from_slice(&v.to_le_bytes());
    }
    let packet = |kind: u8| [&[kind, 0], body.as_slice()].concat();
    assert_eq!(parse_packet(&packet(3)).unwrap().payload, Payload::Gyro(vec![[1, -2, 3]]));
    assert_eq!(parse_packet(&packet(4)).unwrap().payload, Payload::Mag(vec![[1, -2, 3]]));
  }

  #[test]
  fn channels_have_the_right_scales() {
    let channels = Default::default();
    let data = GarminData { channels: &channels, time: Duration::ZERO };
    let scales: Vec<_> = (0..data.num_channels()).map(|channel| AnalogData::scale(&data, channel)).collect();
    assert_eq!(scales, [1.0, 0.001, 0.001, 0.001, 0.1, 0.1, 0.1, 1.0, 1.0, 1.0]);
  }

  #[test]
  fn packets_fill_channels_by_sensor() {
    let mut channels: [Vec<i16>; CHANNEL_COUNT] = Default::default();
    push_vectors(&mut channels, GYR_CHANNEL, vec![[1, 2, 3], [4, 5, 6]]);
    assert_eq!((&channels[GYR_CHANNEL], &channels[MAG_CHANNEL - 1]), (&vec![1, 4], &vec![3, 6]));
    assert!(channels[ACC_CHANNEL].is_empty() && channels[MAG_CHANNEL].is_empty());
  }

  #[test]
  fn parses_accel() {
    let mut bytes = vec![2, 255];
    for v in [1000i16, -1000, 0, -1, 2, 32767] {
      bytes.extend_from_slice(&v.to_le_bytes());
    }
    assert_eq!(
      parse_packet(&bytes),
      Ok(Packet {
        seq: 255,
        payload: Payload::Accel(vec![[1000, -1000, 0], [-1, 2, 32767]]),
      })
    );
  }

  #[test]
  fn rejects_malformed_packets() {
    assert!(parse_packet(&[]).is_err());
    assert!(parse_packet(&[1]).is_err());
    assert!(parse_packet(&[1, 0]).is_err());
    assert!(parse_packet(&[1, 0, 60, 0]).is_err());
    // The heart beat interval packet this replaced.
    assert!(parse_packet(&[5, 0, 1, 2]).is_err());
    assert!(parse_packet(&[2, 0]).is_err());
    assert!(parse_packet(&[2, 0, 1, 2, 3, 4, 5]).is_err());
    assert!(parse_packet(&[3, 0, 1, 2, 3, 4, 5]).is_err());
    assert!(parse_packet(&[4, 0, 1, 2, 3, 4, 5]).is_err());
    assert!(parse_packet(&[9, 0, 0]).is_err());
  }

  fn frame(packet: &[u8]) -> Vec<u8> {
    let mut bytes = vec![packet.len() as u8];
    bytes.extend_from_slice(packet);
    bytes.push(FRAME_END);
    bytes
  }

  #[test]
  fn deframes_packets() {
    let mut bytes = frame(&[1, 2, 3]);
    bytes.extend(frame(&[4]));
    assert_eq!(Deframer::default().push(&bytes), [vec![1, 2, 3], vec![4]]);
  }

  #[test]
  fn deframes_packets_split_across_reads() {
    let bytes = frame(&[1, 2, 3]);
    for split in 0..bytes.len() {
      let mut deframer = Deframer::default();
      let mut packets = deframer.push(&bytes[..split]);
      packets.extend(deframer.push(&bytes[split..]));
      assert_eq!(packets, [vec![1, 2, 3]], "{split}");
    }
  }

  #[test]
  fn locks_on_from_any_offset() {
    // The packet contains the end byte.
    let first = frame(&[FRAME_END, 2, 3]);
    for offset in 1..first.len() {
      let mut bytes = first[offset..].to_vec();
      bytes.extend(frame(&[7, 8]));
      assert_eq!(Deframer::default().push(&bytes), [vec![7, 8]], "{offset}");
    }
  }

  #[test]
  fn skips_corrupt_frames() {
    let mut corrupt = frame(&[1, 2, 3]);
    *corrupt.last_mut().unwrap() ^= 1;
    corrupt.extend(frame(&[7, 8]));
    assert_eq!(Deframer::default().push(&corrupt), [vec![7, 8]]);
    // Invalid lengths.
    assert_eq!(Deframer::default().push(&[0, 200, FRAME_END]), Vec::<Vec<u8>>::new());
  }

  #[test]
  fn deframer_buffer_doesnt_grow_without_frames() {
    let mut deframer = Deframer::default();
    for _ in 0..100 {
      deframer.push(&[0x11; 256]);
    }
    // Bytes that could still be the start of a frame are kept.
    assert!(deframer.buffer.len() <= MAX_FRAME_PAYLOAD + 1, "{}", deframer.buffer.len());
  }

  #[test]
  fn parses_text() {
    let mut bytes = vec![PACKET_TEXT, 0];
    bytes.extend_from_slice(b"connected");
    assert_eq!(parse_packet(&bytes).unwrap().payload, Payload::Text("connected".to_string()));
  }

  // Frames from arduino/thalamus_bridge's encoder.
  #[test]
  #[ignore = "needs a file from the encoder in BRIDGE_FRAMES"]
  fn decodes_bridge_frames() {
    let bytes = std::fs::read(std::env::var("BRIDGE_FRAMES").unwrap()).unwrap();
    let packets = Deframer::default().push(&bytes);
    let payloads: Vec<_> = packets.iter().map(|packet| parse_packet(packet).unwrap().payload).collect();
    assert_eq!(payloads, [
      Payload::HeartRate(72),
      Payload::Gyro(vec![[1000, -1000, 0], [-1, 2, 32767], [-32768, 300, -300]]),
      Payload::Text("connected to 64:a3:37:18:60:80".to_string()),
    ]);
  }

  #[test]
  fn counts_missing_packets_across_wraparound() {
    assert_eq!(missing_packets(5, 5), 0);
    assert_eq!(missing_packets(5, 8), 3);
    assert_eq!(missing_packets(254, 1), 3);
  }

  #[test]
  fn max_packets_fit_default_mtu() {
    assert!(HEADER_SIZE + 3 * VECTOR_SAMPLE_SIZE <= 20);
  }
}
