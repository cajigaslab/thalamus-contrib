use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;
use tokio::sync::oneshot;
use std::time::Duration;
use std::str::FromStr;
use uuid::Uuid;
use futures::stream::StreamExt;
use bytebuffer::ByteReader;
use tokio_util::sync::CancellationToken;
use crate::api::{
  self, THALAMUS_MODALITY_ANALOG, NodeData, AnalogData, Json, MainThreadToken, Node, NodeToken, OnDrop, Request,
  State, StateAction, StateValue, ThalamusAPI, MainThreadOnly, OnDropSend
};
use btleplug::api::{
  Characteristic, Service,
  Central, Manager as _,
  Peripheral as _, ScanFilter, WriteType, CentralEvent, BDAddr};
use btleplug::platform::Peripheral;
use crate::block as blk;
use crate::bluetooth as bt;

const ADC_VDD: f64 = 3.3;
const ADC_VREF: f64 = ADC_VDD / 4.0;
const ADC_GAIN: f64 = 0.25;
const ADC_RESOLUTION_BITS: i32 = 14;

struct SleeveNodeInner {
  api:              ThalamusAPI,
  node_token:       NodeToken,
  _state_connection: OnDrop,
  bluetooth_handle: Option<JoinHandle<()>>,
  main_thread_token: MainThreadToken,
  state: State,
  address: Option<BDAddr>,
  cancel_token: Option<CancellationToken>,
}

pub struct SleeveNode {
    inner: Arc<Mutex<SleeveNodeInner>>,
}

struct SleeveData<'a> {
  intan_channels: &'a Vec::<Vec<i16>>,
  icm_channels: &'a Vec::<Vec<i16>>,
  adc_channels: &'a Vec::<Vec<i16>>,
  time: Duration,
}

enum ChannelType {
  INTAN,
  ICM,
  ADC
}

impl<'a> SleeveData<'a> {
  fn to_type_index(&self, channel: i32) -> (ChannelType, usize) {
    let mut uchannel = usize::try_from(channel).unwrap();
    if uchannel < self.intan_channels.len() {
      return (ChannelType::INTAN, uchannel);
    }
    uchannel -= self.intan_channels.len();

    if uchannel < self.icm_channels.len() {
      return (ChannelType::ICM, uchannel);
    }
    uchannel -= self.icm_channels.len();

    (ChannelType::ADC, uchannel)
  }
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
    match self.to_type_index(channel) {
      (ChannelType::INTAN, i) => {
        self.intan_channels[i].as_slice()
      },
      (ChannelType::ICM, i) => {
        self.icm_channels[i].as_slice()
      },
      (ChannelType::ADC, i) => {
        self.adc_channels[i].as_slice()
      },
    }
  }

  fn num_channels(&self) -> i32 {
    (self.intan_channels.len()
     + self.icm_channels.len()
     + self.adc_channels.len()) as i32
  }

  fn sample_interval(&self, channel: i32) -> std::time::Duration {
    match self.to_type_index(channel) {
      (ChannelType::INTAN, _) => {
        Duration::from_millis(1)
      },
      (ChannelType::ICM, _) => {
        Duration::from_millis(10)
      },
      (ChannelType::ADC, _) => {
        Duration::from_millis(10)
      },
    }
  }

  fn name(
    &self,
    channel: i32,
  ) -> &str {
    match self.to_type_index(channel) {
      (ChannelType::INTAN, i) => {
        match i {
          0 => "INTAN:0",
          1 => "INTAN:1",
          2 => "INTAN:2",
          3 => "INTAN:3",
          4 => "INTAN:4",
          5 => "INTAN:5",
          6 => "INTAN:6",
          7 => "INTAN:7",
          8 => "INTAN:8",
          9 => "INTAN:9",
          10 => "INTAN:10",
          11 => "INTAN:11",
          12 => "INTAN:12",
          13 => "INTAN:13",
          14 => "INTAN:14",
          15 => "INTAN:15",
          _ => panic!("INTAN Channel out of range {i}"),
        }
      },
      (ChannelType::ICM, i) => {
        match i {
          0 => "ACC_X",
          1 => "ACC_Y",
          2 => "ACC_Z",
          3 => "GYRO_X",
          4 => "GYRO_Y",
          5 => "GYRO_Z",
          _ => panic!("ICM Channel out of range {i}"),
        }
      },
      (ChannelType::ADC, i) => {
        match i {
          0 => "ADC:0",
          _ => panic!("ADC Channel out of range {i}"),
        }
      },
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

  fn scale(&self, channel: i32) -> f64 {
    match self.to_type_index(channel) {
      (ChannelType::INTAN, _) => {
        0.000195
      },
      (ChannelType::ICM, _) => {
        1.0
      },
      (ChannelType::ADC, _) => {
        ADC_VREF / ADC_GAIN / (((1 << ADC_RESOLUTION_BITS) - 1) as f64) * 1000.0
      },
    }
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
  ($result:expr, $msg:expr, $return:expr) => {
    match $result {
      Ok(val) => val,
      Err(e) => {
        println!("[{}:{}] {}: {}", file!(), line!(), $msg, e);
        return $return;
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
  ($result:expr, $msg:expr, $return:expr) => {
    match $result {
      Some(val) => val,
      None => {
        println!("[{}:{}] {}", file!(), line!(), $msg);
        return $return;
      }
    }
  };
}

async fn wait<F, T, E>(f: F, cancel_token: &CancellationToken) -> Result<T, String> 
where
  F: std::future::Future<Output = Result<T, E>>,
  E: ToString,
{
  tokio::select! {
    _ = cancel_token.cancelled() => {
      Err("Cancelled".to_string())
    },
    result = f => {
      match result {
        Ok(v) => Ok(v),
        Err(v) => Err(v.to_string()),
      }
    }
  }
}

async fn wait_opt<F, T>(f: F, cancel_token: &CancellationToken) -> Option<T> 
where
  F: std::future::Future<Output = Option<T>>,
{
  tokio::select! {
    _ = cancel_token.cancelled() => {
      None
    },
    result = f => {
      result
    }
  }
}

async fn try_sleep(d: Duration, cancel_token: &CancellationToken) -> Result<(), &str> {
  tokio::select! {
    _ = cancel_token.cancelled() => {
      Err("Cancelled")
    },
    _ = tokio::time::sleep(d) => {
      Ok(())
    }
  }
}
  
fn stop_bluetooth<F: FnOnce() + 'static>(inner: Arc<Mutex<SleeveNodeInner>>, on_stopped: F) {
  let (handle, api, main_thread_token) = {
    let mut lock = inner.lock().unwrap();
    if let Some(cancel) = lock.cancel_token.take() {
      cancel.cancel();
    }
    (lock.bluetooth_handle.take(), lock.api, lock.main_thread_token)
  };
  api.join_task_then(handle, main_thread_token, on_stopped);
}

impl SleeveNodeInner {
  async fn bluetooth(api: api::ThalamusAPIThreadSafe, node_token: NodeToken, target_address: Option<BDAddr>,
                     cancel_token: CancellationToken) {
    let uart_service_uuid = Uuid::parse_str("6e400001-b5a3-f393-e0a9-e50e24dcca9e").unwrap();
    let uart_rx_char_uuid = Uuid::parse_str("6E400002-B5A3-F393-E0A9-E50E24DCCA9E").unwrap();
    let uart_tx_char_uuid = Uuid::parse_str("6E400003-B5A3-F393-E0A9-E50E24DCCA9E").unwrap();

    let manager = get_result!(wait(bt::get_manager(), &cancel_token).await, "Manager init failed");
    let adapters = get_result!(wait(manager.adapters(), &cancel_token).await, "Failed to get adapters");
    let central = get_option!(adapters.into_iter().nth(0), "No adapters found");
    get_result!(wait(central.start_scan(ScanFilter::default()), &cancel_token).await, "start_scan failed");

    //get_result!(central.clear_peripherals().await, "clear_peripherals failed");

    let events_central = central.clone();
    let events_cancel_token = cancel_token.clone();
    let (ptx, prx) = oneshot::channel::<Peripheral>();
    let disconnect_notify = Arc::new(tokio::sync::Notify::new());

    let mut notify_opt = Some(disconnect_notify.clone());
    let scan_handle = api.tokio().as_ref().unwrap().spawn(async move {
      let mut ptx_opt = Some(ptx);
      let mut selected_id = None;

      let mut handle_peripheral = async |peripheral: Peripheral| -> Result<bool, String> {
        let properties_opt = match wait(peripheral.properties(), &events_cancel_token).await {
          Ok(v) => v,
          Err(v) => return Err(v.to_string()),
        };
        let properties = match properties_opt {
          Some(v) => v,
          None => return Err("Failed to unwrap peripheral properties".to_string()),
        };
        let address = properties.address;
        let name = properties.local_name.unwrap_or("no name".to_string());
        println!("{name} {address}");
        match target_address {
          Some(target) => {
            if address == target {
              if let Some(ptx) = ptx_opt.take() {
                return match ptx.send(peripheral) {
                  Ok(_) => Ok(true),
                  Err(_) => Err("Failed to send".to_string()),
                };
              }
            }
          },
          None => {
            if name == "NORA_INTAN_RHD_ICM" {
              if let Some(ptx) = ptx_opt.take() {
                return match ptx.send(peripheral) {
                  Ok(_) => Ok(true),
                  Err(_) => Err("Failed to send".to_string()),
                };
              }
            }
          }
        }
        return Ok(false);
      };

      let peripherals = get_result!(wait(events_central.peripherals(), &events_cancel_token).await, "Failed to query existing peripherals");
      println!("Previous peripherals");
      for peripheral in peripherals {
        let pid = peripheral.id();
        let found = get_result!(wait(handle_peripheral(peripheral), &events_cancel_token).await, "Failed to handle peripheral");
        if found {
          selected_id = Some(pid);
          break;
        }
      }
      
      println!("scanning peripherals");
      let mut events = get_result!(wait(events_central.events(), &events_cancel_token).await, "Failed to get events");
      while let Some(e) = wait_opt(events.next(), &events_cancel_token).await {
        match e {
          CentralEvent::DeviceDiscovered(id) => {
            let peripheral = get_result!(wait(events_central.peripheral(&id), &events_cancel_token).await, "Failed to get peripheral");
            let found = get_result!(wait(handle_peripheral(peripheral), &events_cancel_token).await, "Failed to handle peripheral");
            if found {
              selected_id = Some(id);
            }
          },
          CentralEvent::DeviceDisconnected(id) => {
            if Some(id) == selected_id {
              println!("Sleeve disconnected");
              if let Some(notify) = notify_opt.take() {
                notify.notify_one();
              }
            }
          }
          _ => {}
        }
      }
    });
    let _dropper = tokio_util::task::AbortOnDropHandle::new(scan_handle);

    let peripheral = match wait(prx, &cancel_token).await {
      Ok(v) => {
        get_result!(central.stop_scan().await, "stop_scan failed");
        v
      },
      Err(_) => {
        get_result!(central.stop_scan().await, "stop_scan failed");
        return;
      }
    };

    get_result!(wait(peripheral.connect(), &cancel_token).await, "Connect failed");
    println!("MTU = {}", peripheral.mtu());

    let disconnect_peripheral = peripheral.clone();
    let _disconnect = OnDropSend::new(move || {
      api.tokio().as_ref().unwrap().spawn(async move {
        let _ = disconnect_peripheral.disconnect().await;
      });
    });

    get_result!(wait(peripheral.discover_services(), &cancel_token).await, "Failed to discover services");
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
    get_result!(wait(peripheral.subscribe(&tx_char), &cancel_token).await, "Failed to subscript to TX");

    let unsubscribe_peripheral = peripheral.clone();
    let _unsubscribe = OnDropSend::new(move || {
      api.tokio().as_ref().unwrap().spawn(async move {
        let _ = unsubscribe_peripheral.unsubscribe(&tx_char).await;
      });
    });

    //On Android running the following commands with no pause between them frequently breaks the
    //connection.  On Android we space the commands out by 800 ms, Desktop and Laptops might need
    //less or even none at all.
    //let pause = Duration::from_millis(800);
    let pause = Duration::from_millis(100);

    {
      println!("Set ICM Mask");
      get_result!(try_sleep(pause, &cancel_token).await, "Cancelled");
      let block = blk::Block::cmd(blk::ID_SET_CHANNEL_MASK, &[2, 0, 0, 0, 0x3F]);
      get_result!(wait(peripheral.write(&rx_char, &block.encode(), WriteType::WithoutResponse), &cancel_token).await, "Write failed");
    }
    {
      println!("Set INTAN Mask");
      get_result!(try_sleep(pause, &cancel_token).await, "Cancelled");
      let block = blk::Block::cmd(blk::ID_SET_CHANNEL_MASK, &[4, 0, 0, 0x0F, 0xF0]);
      get_result!(wait(peripheral.write(&rx_char, &block.encode(), WriteType::WithoutResponse), &cancel_token).await, "Write failed");
    }
    {
      println!("Set ADC Mask");
      get_result!(try_sleep(pause, &cancel_token).await, "Cancelled");
      let block = blk::Block::cmd(blk::ID_SET_CHANNEL_MASK, &[5, 0, 0, 0, 0x01]);
      get_result!(wait(peripheral.write(&rx_char, &block.encode(), WriteType::WithoutResponse), &cancel_token).await, "Write failed");
    }


    {
      println!("Set ICM Sample Rate");
      get_result!(try_sleep(pause, &cancel_token).await, "Cancelled");
      let block = blk::Block::cmd(blk::ID_SET_SAMPLE_RATE, &[2, 0]);
      get_result!(wait(peripheral.write(&rx_char, &block.encode(), WriteType::WithoutResponse), &cancel_token).await, "Write failed");
    }
    {
      println!("Set INTAN Sample Rate");
      get_result!(try_sleep(pause, &cancel_token).await, "Cancelled");
      let block = blk::Block::cmd(blk::ID_SET_SAMPLE_RATE, &[4, 0x13]);
      get_result!(wait(peripheral.write(&rx_char, &block.encode(), WriteType::WithoutResponse), &cancel_token).await, "Write failed");
    }
    {
      println!("Set ADC Sample Rate");
      get_result!(try_sleep(pause, &cancel_token).await, "Cancelled");
      let block = blk::Block::cmd(blk::ID_SET_SAMPLE_RATE, &[5, 0]);
      get_result!(wait(peripheral.write(&rx_char, &block.encode(), WriteType::WithoutResponse), &cancel_token).await, "Write failed");
    }

    {
      println!("Enable ICM");
      get_result!(try_sleep(pause, &cancel_token).await, "Cancelled");
      let block = blk::Block::cmd(blk::ID_ENABLE, &[2, 0x01]);
      get_result!(wait(peripheral.write(&rx_char, &block.encode(), WriteType::WithoutResponse), &cancel_token).await, "Write failed");
    }
    {
      println!("Enable INTAN");
      get_result!(try_sleep(pause, &cancel_token).await, "Cancelled");
      let block = blk::Block::cmd(blk::ID_ENABLE, &[4, 0x01]);
      get_result!(wait(peripheral.write(&rx_char, &block.encode(), WriteType::WithoutResponse), &cancel_token).await, "Write failed");
    }
    {
      println!("Enable ADC");
      get_result!(try_sleep(pause, &cancel_token).await, "Cancelled");
      let block = blk::Block::cmd(blk::ID_ENABLE, &[5, 0x01]);
      get_result!(wait(peripheral.write(&rx_char, &block.encode(), WriteType::WithoutResponse), &cancel_token).await, "Write failed");
    }
    
    let mut next_first_point: i32 = 0;
    let mut notifications = get_result!(wait(peripheral.notifications(), &cancel_token).await, "Failed to get notifications");
    let mut intan_channels = Vec::<Vec<i16>>::new();
    intan_channels.resize(16, Vec::<i16>::default());
    let mut adc_channels = Vec::<Vec<i16>>::new();
    adc_channels.resize(1, Vec::<i16>::default());
    let mut icm_channels = Vec::<Vec<i16>>::new();
    icm_channels.resize(6, Vec::<i16>::default());

    let intan_first_channel = 4;
    let intan_num_channels = 8;

    loop {
      tokio::select! {
        _ = disconnect_notify.notified() => {
          println!("Disconnected");
          break;
        }
        _ = cancel_token.cancelled() => {
          println!("Cancelled");
          break
        }
        Some(n) = notifications.next() => {
          //println!("notification {:?}", n);
          let blocks = get_result!(blk::decode_block_packet(&n.value), "Failed to parse blocks");
          for block in blocks {
            //println!("block {block:?}");
            match block.block_id {
              2 => {
                let mut reader = ByteReader::from_bytes(block.data);
                reader.set_endian(bytebuffer::Endian::BigEndian);
                let mut index = 0;
                while reader.get_rpos() < reader.len() {
                  let raw = get_result!(reader.read_i16(), "Failed to read i16");
                  icm_channels[index].push(raw);
                  
                  index = (index + 1) % icm_channels.len();
                }

                let _ = api.ready_offmain(&SleeveData {
                  intan_channels: &intan_channels,
                  icm_channels: &icm_channels,
                  adc_channels: &adc_channels,
                  time: api.time()
                }, &node_token);
                icm_channels.iter_mut().for_each(|c| c.clear());
              }
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
                reader.set_endian(bytebuffer::Endian::BigEndian);
                let mut index = block.first_channel_sampled as usize;
                while reader.get_rpos() < reader.len() {
                  let raw = get_result!(reader.read_u16(), "Failed to read u16") as i32;
                  let temp = raw - 0x8000;
                  intan_channels[index].push(temp as i16);
                  
                  index += 1;
                  if index == intan_first_channel + intan_num_channels {
                    index = intan_first_channel;
                  }
                }

                let _ = api.ready_offmain(&SleeveData {
                  intan_channels: &intan_channels,
                  icm_channels: &icm_channels,
                  adc_channels: &adc_channels,
                  time: api.time()
                }, &node_token);
                intan_channels.iter_mut().for_each(|c| c.clear());
              }
              5 => { 
                let mut reader = ByteReader::from_bytes(block.data);
                reader.set_endian(bytebuffer::Endian::LittleEndian);
                let mut index = 0;
                while reader.get_rpos() < reader.len() {
                  let raw = get_result!(reader.read_i16(), "Failed to read i16");
                  adc_channels[index].push(raw);
                  
                  index = (index + 1) % adc_channels.len();
                }

                let _ = api.ready_offmain(&SleeveData {
                  intan_channels: &intan_channels,
                  icm_channels: &icm_channels,
                  adc_channels: &adc_channels,
                  time: api.time()
                }, &node_token);
                adc_channels.iter_mut().for_each(|c| c.clear());
              }
              _ => { println!("Other"); }
            }
            //println!("{} {:?}\n", block.data.len(), block.data);
          }
        }
      }
    }
    println!("bluetooth end");
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
            let address = lock.address.clone();
            let wrapped_state = MainThreadOnly::new(lock.state.clone(), lock.main_thread_token);
            let cancel_token = CancellationToken::new();
            lock.cancel_token = Some(cancel_token.clone());
            lock.bluetooth_handle = Some(api.tokio().as_ref().unwrap().spawn(async move {
              SleeveNodeInner::bluetooth(api, node_token, address, cancel_token).await;
              api.post_to_main(|main_thread_token| {
                let state = wrapped_state.take(main_thread_token);
                state.set(api::StateKey::String("Running".to_string()), api::StateValue::Bool(false));
              });
            }));
          });
        } else {
          stop_bluetooth(me, || {});
        }
      }
      "Address" => {
        if let StateValue::String(address) = value {
          me.lock().unwrap().address = BDAddr::from_str(address.as_str()).ok();
        }
      }
      _ => {}
    }
  }
}

impl Node for SleeveNode {
  fn modalities(&self) -> u32 {
    THALAMUS_MODALITY_ANALOG
  }

  fn signals_offmain(&self) -> bool {
    true
  }

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

      let _state_connection = state.connect(state_callback);
      Mutex::new(SleeveNodeInner {
        api, node_token, _state_connection, bluetooth_handle: None, main_thread_token,
        state: state.clone(), address: None, cancel_token: None
      })
    });

    state.recap_with(|source: State, action: StateAction, key: StateValue, value: StateValue| {
      SleeveNodeInner::on_state(inner.clone(), source, action, key, value);
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
