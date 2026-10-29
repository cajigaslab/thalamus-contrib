// Receives heart rate and accelerometer data from the Connect IQ app in
// garmin/. Connect IQ apps can only be BLE centrals, so this node is the
// peripheral: it advertises GARMIN_SERVICE_UUID and the watch writes packets
// (see parse_packet) to GARMIN_DATA_CHAR_UUID. btleplug is central-only, so
// this uses ble-peripheral-rust.
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
use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::{Uuid, uuid};

// Must match garmin/source/ThalamusProfile.mc.
pub const GARMIN_SERVICE_UUID: Uuid = uuid!("7a4b0001-5c2e-4f6a-9d3b-8e1f2a6c4d90");
pub const GARMIN_DATA_CHAR_UUID: Uuid = uuid!("7a4b0002-5c2e-4f6a-9d3b-8e1f2a6c4d90");

const PACKET_HEART_RATE: u8 = 1;
const PACKET_ACCEL: u8 = 2;
const HEADER_SIZE: usize = 2;
const ACCEL_SAMPLE_SIZE: usize = 6;

// The watch samples the accelerometer at this rate (ACCEL_RATE_HZ in
// garmin/source/SensorSource.mc).
const ACCEL_INTERVAL: Duration = Duration::from_millis(40);
// Heart rate is sent once a second.
const HEART_RATE_INTERVAL: Duration = Duration::from_secs(1);

const CHANNEL_NAMES: [&str; 4] = ["HR", "ACC_X", "ACC_Y", "ACC_Z"];
const HR_CHANNEL: usize = 0;
const ACC_CHANNEL: usize = 1;

#[derive(Debug, PartialEq)]
pub enum Payload {
  // Beats per minute, 0 if the watch has no reading.
  HeartRate(u8),
  // x, y, z samples in milli-g, oldest first.
  Accel(Vec<[i16; 3]>),
}

#[derive(Debug, PartialEq)]
pub struct Packet {
  pub seq: u8,
  pub payload: Payload,
}

// Packets are at most 20 bytes, the ATT payload at the default MTU, all
// little endian:
//   byte 0: type (1 = heart rate, 2 = accelerometer)
//   byte 1: sequence number, incremented per packet of any type, wrapping
//   heart rate:    byte 2 = bpm (u8)
//   accelerometer: 1 to 3 samples of i16 x, y, z in milli-g
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
    PACKET_ACCEL => {
      if body.is_empty() || body.len() % ACCEL_SAMPLE_SIZE != 0 {
        return Err(format!("accelerometer packet has {} payload bytes", body.len()));
      }
      let samples = body
        .chunks_exact(ACCEL_SAMPLE_SIZE)
        .map(|s| {
          [
            i16::from_le_bytes([s[0], s[1]]),
            i16::from_le_bytes([s[2], s[3]]),
            i16::from_le_bytes([s[4], s[5]]),
          ]
        })
        .collect();
      Payload::Accel(samples)
    }
    other => return Err(format!("unknown packet type {other}")),
  };
  Ok(Packet { seq, payload })
}

// Packets lost between `expected` and `seq`, counting wraparound.
fn missing_packets(expected: u8, seq: u8) -> u8 {
  seq.wrapping_sub(expected)
}

struct GarminData<'a> {
  channels: &'a [Vec<i16>; 4],
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
      ACCEL_INTERVAL
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

  // Heart rate in bpm, acceleration in g.
  fn scale(&self, channel: i32) -> f64 {
    if channel as usize == HR_CHANNEL { 1.0 } else { 0.001 }
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

  let mut channels: [Vec<i16>; 4] = Default::default();
  let mut expected_seq: Option<u8> = None;

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

    let packet = match parse_packet(&value) {
      Ok(packet) => packet,
      Err(e) => {
        report(&api, &format!("dropping packet from {}: {e}", request.client));
        continue;
      }
    };
    if let Some(expected) = expected_seq {
      let missing = missing_packets(expected, packet.seq);
      if missing != 0 {
        report(&api, &format!("missed {missing} packets"));
      }
    }
    expected_seq = Some(packet.seq.wrapping_add(1));

    match packet.payload {
      Payload::HeartRate(bpm) => channels[HR_CHANNEL].push(i16::from(bpm)),
      Payload::Accel(samples) => {
        for sample in samples {
          for (axis, value) in sample.into_iter().enumerate() {
            channels[ACC_CHANNEL + axis].push(value);
          }
        }
      }
    }

    let data = GarminData { channels: &channels, time: api.time() };
    if signaler.ready(&data).is_err() {
      break Ok(());
    }
    channels.iter_mut().for_each(Vec::clear);
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
    if let Err(e) = serve(api, &signaler, &ble_name, &cancel_token).await {
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
    assert_eq!(
      parse_packet(&[1, 7, 72]),
      Ok(Packet { seq: 7, payload: Payload::HeartRate(72) })
    );
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
    assert!(parse_packet(&[2, 0]).is_err());
    assert!(parse_packet(&[2, 0, 1, 2, 3, 4, 5]).is_err());
    assert!(parse_packet(&[9, 0, 0]).is_err());
  }

  #[test]
  fn counts_missing_packets_across_wraparound() {
    assert_eq!(missing_packets(5, 5), 0);
    assert_eq!(missing_packets(5, 8), 3);
    assert_eq!(missing_packets(254, 1), 3);
  }

  #[test]
  fn max_accel_packet_fits_default_mtu() {
    assert!(HEADER_SIZE + 3 * ACCEL_SAMPLE_SIZE <= 20);
  }
}
