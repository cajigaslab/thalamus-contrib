use std::cell::{RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use crate::api::{self, AnalogData, Json, MainThreadToken, Node, NodeToken, OnDrop, Request, State, StateAction, StateValue, ThalamusAPI};

use btleplug::api::{
  Characteristic, Service, ValueNotification,
  bleuuid::uuid_from_u16, Central, Manager as _,
  Peripheral as _, ScanFilter, WriteType, CentralEvent};
use btleplug::platform::{Adapter, Manager, Peripheral};
use chrono::Duration;
use crate::block as blk;

struct SleeveNodeInner {
  api:               ThalamusAPI,
  node_token:        NodeToken,
  state:             State,
  state_connection:  OnDrop,
  bluetooth_handle: Some(JoinHandle<()>),
}

pub struct SleeveNode {
    inner: Arc<Mutex<SleeveNodeInner>>,
}

struct SleeveData {
  channels: Vec::<Vec<i16>>,
  time: Duration,
}

impl NodeData for SleeveData {
    fn time(&self) -> Duration {
        self.time
    }
    fn analog(&self) -> Option<&dyn AnalogData> { 
      Some(self)
    }
}

impl AnalogData for SleeveData {
  fn short_data(
            &self,
            channel: i32,
        ) -> &[i16] {
    self.channels[channel].as_slice()
  }

  fn num_channels(&self) -> i32 {
    self.channels.len() as i32
  }

  fn sample_interval(&self, channel: i32) -> std::time::Duration {
    Duration::from_millis(1)
  }

  fn name(
    &self,
    channel: ::std::os::raw::c_int,
  ) -> &str {
    match channel {
      0 => "0",
      1 => "1",
      2 => "2",
      3 => "3",
      4 => "4",
      5 => "5",
      6 => "6",
      7 => "7",
      8 => "8",
      9 => "9",
      10 => "10",
      11 => "11",
      12 => "12",
      13 => "13",
      14 => "14",
      15 => "15",
      _ => panic!("Unexpected channel: {channel}")
    }
  }

  fn is_short_data(&self) -> bool{
    true
  }

  fn is_transformed(&self) -> bool {
    true
  }

  fn offset(&self, _channel: i32) -> f64 {
    0.0
  }

  fn scale(&self, _channel: i32) -> f64 {
    0.000195
  }
}

impl SleeveNodeInner {
  async fn bluetooth(api: api::ThalamusAPIThreadSafe, node_token: NodeToken) {
    let uart_service_uuid = Uuid::parse_str("6e400001-b5a3-f393-e0a9-e50e24dcca9e")?;
    let uart_rx_char_uuid = Uuid::parse_str("6E400002-B5A3-F393-E0A9-E50E24DCCA9E")?;
    let uart_tx_char_uuid = Uuid::parse_str("6E400003-B5A3-F393-E0A9-E50E24DCCA9E")?;

    let manager = Manager::new().await.unwrap();
    let adapters = manager.adapters().await?;
    let central = adapters.into_iter().nth(0).unwrap();

    central.start_scan(ScanFilter::default()).await?;
    let mut events = central.events().await?;
    let mut peripheral_opt: Option<Peripheral> = None;
    while let Some(e) = events.next().await {
      match e {
        CentralEvent::DeviceDiscovered(id) => {
          let peripheral = central.peripheral(&id).await?;
          let properties = peripheral.properties().await?.unwrap();
          let address = properties.address;
          let name = properties.local_name.unwrap_or("no name".to_string());
          println!("{name} {address}");
          if name == "NORA_INTAN_RHD_ICM" {
            peripheral_opt = Some(peripheral);
            break;
          }
          //if peripheral.
        },
        _ => {}
      }
    }

    let peripheral = peripheral_opt.unwrap();
    peripheral.connect().await?;
    println!("MTU = {}", peripheral.mtu());

    peripheral.discover_services().await?;
    let mut uart_service_opt: Option<Service> = None;
    for service in peripheral.services() {
      if service.uuid == uart_service_uuid {
        uart_service_opt = Some(service);
        break;
      }
    }

    let uart_service = uart_service_opt.unwrap();

    let mut rx_char_opt: Option<Characteristic> = None;
    let mut tx_char_opt: Option<Characteristic> = None;
    for char in uart_service.characteristics {
      if char.uuid == uart_rx_char_uuid {
        rx_char_opt = Some(char);
      } else if char.uuid == uart_tx_char_uuid {
        tx_char_opt = Some(char);
      }
    }

    let rx_char = rx_char_opt.unwrap();
    let tx_char = tx_char_opt.unwrap();

    //peripheral.subscribe(&rx_char).await?;
    peripheral.subscribe(&tx_char).await?;

    let pause = Duration::from_millis(800);

    {
      tokio::time::sleep(pause).await;
      let block = blk::Block::cmd(blk::ID_SET_CHANNEL_MASK, &[4, 0, 0, 0x0F, 0xF0]);
      peripheral.write(&rx_char, &block.encode(), WriteType::WithoutResponse).await?;
    }

    {
      tokio::time::sleep(pause).await;
      let block = blk::Block::cmd(blk::ID_SET_SAMPLE_RATE, &[4, 0x13]);
      peripheral.write(&rx_char, &block.encode(), WriteType::WithoutResponse).await?;
    }

    {
      tokio::time::sleep(pause).await;
      let block = blk::Block::cmd(blk::ID_ENABLE, &[4, 0x01]);
      peripheral.write(&rx_char, &block.encode(), WriteType::WithoutResponse).await?;
    }
    
    let mut next_first_point: i32 = 0;
    let mut notifications = peripheral.notifications().await?;
    while let Some(n) = notifications.next().await {
      let blocks = blk::decode_block_packet(&n.value)?;
      for block in blocks {
        match block.block_id {
          2 => { println!("ICM"); }
          4 => { 
            let raw_stride = block.data.len()/8/2;
            let data = block.data;

            let total_bytes = data.len();
            let points_avail = total_bytes / 2;            // 16-bit values available (after trim)
            let full_frames  = points_avail / raw_stride;
            let read_points  = full_frames * raw_stride;
            if read_points == 0 { break; }

            let mut missing = (block.first_point_idx as i32) - next_first_point;
            while missing < 0 {
              missing += 0x100;
            }

            next_first_point = (block.first_point_idx as i32) + i32::try_from(full_frames).unwrap();

            let mut channels = Vec::<Vec<i16>>::new();
            channels.resize(8, Vec::<i16>::default());

            let mut reader = ByteReader::from_bytes(data);
            let mut channel = (block.first_channel_sampled - 4) as usize;
            println!("{}", channel);
            while reader.get_rpos() < reader.len() {
              let temp = (reader.read_u16()? as i32) - 0x8000;
              channels[channel].push(temp as i16);
              channel = (channel + 1) % channels.len();
            }

            api.ready_offmain(data, &node_token);
            let lock = self.lock().unwrap();
            lock.api.ready(SleeveData {
              channels, time: api.time()
            }, token);
          }
          5 => { println!("ADC"); }
          _ => { println!("Other"); }
        }
        //println!("{} {:?}\n", block.data.len(), block.data);
      }
    }
  }

  fn on_state(self: Arc<Mutex<SleeveNodeInner>>, _source: State, _action: StateAction, key: StateValue, value: StateValue) {
    let StateValue::String(key_str) = key else {
      return;
    };
    match key_str.as_str() {
      "Running" => {
        if value == StateValue::Bool(true) {
          let lock = self.lock().unwrap();
          lock.bluetooth_handle = Some(lock.api.tokio().unwrap().spawn(async {
            SleeveNodeInner::bluetooth(self.api, lock.node_token).await
          }));
        } else {
          let lock = self.lock().unwrap();
          if let Some(bluetooth) = lock.bluetooth_handle.take() {
            bluetooth.abort();
          }
        }
      }
      _ => {}
    }
  }
}

impl Node for SleeveNode {
  fn process(&self, handle: Request, _request: Json) {
    let api = self.inner.lock().unwrap().api;
    handle.respond(&Json::from_string(api, "null"));
  }

  fn new(api: ThalamusAPI, node_token: NodeToken, state: State, main_thread_token: MainThreadToken) -> Self {
    let inner = Arc::new_cyclic(|weak: &std::sync::Weak<Mutex<SleeveNodeInner>>| {
      let weak2 = weak.clone();
      let state_callback = move |source: State, action: StateAction, key: StateValue, value: StateValue| {
        if let Some(strong) = weak2.upgrade() {
          SleeveNodeInner::on_state(strong, source, action, key, value);
        }
      };

      let state_connection = state.connect(state_callback);
      Mutex::new(SleeveNodeInner {
        api, node_token, state, state_connection, bluetooth_handle: None
      })
    });

    SleeveNode {
      inner
    }
  }

  fn predrop(&self, token: api::PredropToken) {
    let inner = self.inner.clone();
    lock.api.tokio().unwrap().spawn(async {
      let lock = inner.lock().unwrap();
      if let Some(bluetooth) = lock.bluetooth_handle.take() {
        bluetooth.abort();
      }
      token.ready();
    });
  }

}
