use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;
use std::time::Duration;
use uuid::Uuid;
use futures::stream::StreamExt;
use bytebuffer::ByteReader;
use crate::api::{self, NodeData, AnalogData, Json, MainThreadToken, Node, NodeToken, OnDrop, Request, State, StateAction, StateValue, ThalamusAPI};

use btleplug::api::{
  Characteristic, Service,
  Central, Manager as _,
  Peripheral as _, ScanFilter, WriteType, CentralEvent};
use btleplug::platform::{Manager, Peripheral};
use crate::block as blk;

struct SleeveNodeInner {
  api:              ThalamusAPI,
  node_token:       NodeToken,
  state:            State,
  state_connection: OnDrop,
  bluetooth_handle: Option<JoinHandle<()>>,
  main_thread_token: MainThreadToken,
}

pub struct SleeveNode {
    inner: Arc<Mutex<SleeveNodeInner>>,
}

struct SleeveData<'a> {
  channels: &'a Vec::<Vec<i16>>,
  time: Duration,
}

impl<'a> NodeData for SleeveData<'a> {
    fn time(&self) -> Duration {
        self.time
    }
    fn analog(&self) -> Option<&dyn AnalogData> { 
      Some(self)
    }
}

impl<'a> AnalogData for SleeveData<'a> {
  fn short_data(
            &self,
            channel: i32,
        ) -> &[i16] {
    self.channels[channel as usize].as_slice()
  }

  fn num_channels(&self) -> i32 {
    self.channels.len() as i32
  }

  fn sample_interval(&self, _channel: i32) -> std::time::Duration {
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

macro_rules! get_result {
  ($result:expr, $msg:expr) => {
    match $result {
      Ok(val) => val,
      Err(e) => {
        println!("[{}:{}] {}: {}", file!(), line!(), $msg, e);
        return;
      }
    }
  };
}

macro_rules! get_option {
  ($result:expr, $msg:expr) => {
    match $result {
      Some(val) => val,
      None => {
        println!("[{}:{}] {}", file!(), line!(), $msg);
        return;
      }
    }
  };
}
  
fn stop_bluetooth<F: FnOnce() + 'static>(inner: Arc<Mutex<SleeveNodeInner>>, on_stopped: F) {
  let (handle, api, main_thread_token) = {
    let mut lock = inner.lock().unwrap();
    (lock.bluetooth_handle.take(), lock.api, lock.main_thread_token)
  };
  if let Some(h) = handle.as_ref() {
    h.abort();
  }
  api.join_task_then(handle, main_thread_token, on_stopped);
}

impl SleeveNodeInner {
  async fn bluetooth(api: api::ThalamusAPIThreadSafe, node_token: NodeToken) {
    let uart_service_uuid = Uuid::parse_str("6e400001-b5a3-f393-e0a9-e50e24dcca9e").unwrap();
    let uart_rx_char_uuid = Uuid::parse_str("6E400002-B5A3-F393-E0A9-E50E24DCCA9E").unwrap();
    let uart_tx_char_uuid = Uuid::parse_str("6E400003-B5A3-F393-E0A9-E50E24DCCA9E").unwrap();

    let manager = get_result!(Manager::new().await, "Manager init failed");
    let adapters = get_result!(manager.adapters().await, "Failed to get adapters");
    let central = get_option!(adapters.into_iter().nth(0), "No adapters found");
    get_result!(central.start_scan(ScanFilter::default()).await, "start_scan failed");

    let mut events = get_result!(central.events().await, "Failed to get events");
    let mut peripheral_opt: Option<Peripheral> = None;
    while let Some(e) = events.next().await {
      match e {
        CentralEvent::DeviceDiscovered(id) => {
          let peripheral = get_result!(central.peripheral(&id).await, "Failed to get peripheral");
          let properties_opt = get_result!(peripheral.properties().await, "Failed to get peripheral properties");
          let properties = get_option!(properties_opt, "Failed to get peripheral properties");
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

    let peripheral = get_option!(peripheral_opt, "No peripheral found");
    get_result!(peripheral.connect().await, "Connect failed");
    println!("MTU = {}", peripheral.mtu());

    get_result!(peripheral.discover_services().await, "Failed to discover services");
    let mut uart_service_opt: Option<Service> = None;
    for service in peripheral.services() {
      if service.uuid == uart_service_uuid {
        uart_service_opt = Some(service);
        break;
      }
    }

    let uart_service = get_option!(uart_service_opt, "UART service not found");

    let mut rx_char_opt: Option<Characteristic> = None;
    let mut tx_char_opt: Option<Characteristic> = None;
    for char in uart_service.characteristics {
      if char.uuid == uart_rx_char_uuid {
        rx_char_opt = Some(char);
      } else if char.uuid == uart_tx_char_uuid {
        tx_char_opt = Some(char);
      }
    }

    let rx_char = get_option!(rx_char_opt, "No RX found");
    let tx_char = get_option!(tx_char_opt, "No TX found");

    //peripheral.subscribe(&rx_char).await?;
    get_result!(peripheral.subscribe(&tx_char).await, "Failed to subscript to TX");

    let pause = Duration::from_millis(800);

    {
      tokio::time::sleep(pause).await;
      let block = blk::Block::cmd(blk::ID_SET_CHANNEL_MASK, &[4, 0, 0, 0x0F, 0xF0]);
      get_result!(peripheral.write(&rx_char, &block.encode(), WriteType::WithoutResponse).await, "Write failed");
    }

    {
      tokio::time::sleep(pause).await;
      let block = blk::Block::cmd(blk::ID_SET_SAMPLE_RATE, &[4, 0x13]);
      get_result!(peripheral.write(&rx_char, &block.encode(), WriteType::WithoutResponse).await, "Write failed");
    }

    {
      tokio::time::sleep(pause).await;
      let block = blk::Block::cmd(blk::ID_ENABLE, &[4, 0x01]);
      get_result!(peripheral.write(&rx_char, &block.encode(), WriteType::WithoutResponse).await, "Write failed");
    }
    
    let mut next_first_point: i32 = 0;
    let mut notifications = get_result!(peripheral.notifications().await, "Failed to get notifications");
    let mut channels = Vec::<Vec<i16>>::new();
    channels.resize(8, Vec::<i16>::default());
    while let Some(n) = notifications.next().await {
      let blocks = get_result!(blk::decode_block_packet(&n.value), "Failed to parse blocks");
      for block in blocks {
        match block.block_id {
          2 => { println!("ICM"); }
          4 => { 
            let num_channels = 8;
            let num_samples = block.data.len()/2/num_channels;
            if num_samples == 0 { break; }

            let data = block.data;

            let mut missing = (block.first_point_idx as i32) - next_first_point;
            while missing < 0 {
              missing += 0x100;
            }
            if missing != 0 {
              println!("missing {missing}");
            }

            next_first_point = (block.first_point_idx as i32) + i32::try_from(num_samples).unwrap();

            let mut reader = ByteReader::from_bytes(data);
            let mut index = (block.first_channel_sampled - 4) as usize;
            while reader.get_rpos() < reader.len() {
              let raw = get_result!(reader.read_u16(), "Failed to read u16") as i32;
              let temp = raw - 0x8000;
              channels[index].push(temp as i16);
              index = (index + 1) % channels.len();
            }

            api.ready_offmain(&SleeveData {
              channels: &channels, time: api.time()
            }, &node_token);
          }
          5 => { println!("ADC"); }
          _ => { println!("Other"); }
        }
        //println!("{} {:?}\n", block.data.len(), block.data);
      }
    }
  }


  fn on_state(me: Arc<Mutex<SleeveNodeInner>>, _source: State, _action: StateAction, key: StateValue, value: StateValue) {
    let StateValue::String(key_str) = key else {
      return;
    };
    match key_str.as_str() {
      "Running" => {
        if value == StateValue::Bool(true) {
          stop_bluetooth(me.clone(), move || {
            let mut lock = me.lock().unwrap();
            let api = lock.api.thread_safe();
            let node_token = lock.node_token.clone();
            lock.bluetooth_handle = Some(api.tokio().as_ref().unwrap().spawn(async move {
              SleeveNodeInner::bluetooth(api, node_token).await
            }));
          });
        } else {
          stop_bluetooth(me, || {});
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
        api, node_token, state, state_connection, bluetooth_handle: None, main_thread_token,
      })
    });

    SleeveNode {
      inner
    }
  }

  fn predrop(&self, token: api::PredropToken) {
    stop_bluetooth(self.inner.clone(), move || {
      token.ready();
    });
  }
}
