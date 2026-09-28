use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::api::{
  ImageFormat,
  MainThreadToken, Node, NodeConsts, NodeSelector, NodeToken, OffMainSignaler, OnDrop, State, StateAction, StateValue, THALAMUS_MODALITY_IMAGE, ThalamusAPI,
};
use crate::image_converter::{Converter, ConverterParams};

#[derive(Clone,Debug)]
struct ParamsHolder {
  params: ConverterParams,
  dirty: bool,
}

pub struct ImageConverterNode {
  api: ThalamusAPI,
  params: Arc<Mutex<ParamsHolder>>,
  state_connection: Option<OnDrop>,
  source_connection: Option<OnDrop>,
  data_connection: Option<OnDrop>,
  signaler: Arc<OffMainSignaler>,
  converter: Arc<Mutex<Converter>>,
  notify: Arc<Notify>,
  dropping: Arc<AtomicBool>,
}

impl NodeConsts for ImageConverterNode {
  const MODALITIES: u32 = THALAMUS_MODALITY_IMAGE;
  const SIGNALS_OFFMAIN: bool = true;
}

impl ImageConverterNode {
  fn on_state(rc: Rc<RefCell<Self>>, _source: State, _action: StateAction, key: StateValue, value: StateValue) {
    let StateValue::String(key_str) = key else {
      return;
    };

    let this = rc.borrow_mut();
    match key_str.as_str() {
      "Format" => {
        let StateValue::String(v) = value else {
          return;
        };

        let mut lock = this.params.lock().unwrap();
        lock.dirty = true;
        lock.params.format = match v.to_uppercase().as_str() {
          "GRAY" => Some(ImageFormat::Gray),
          "RGB" => Some(ImageFormat::RGB), 
          "YUYV422" => Some(ImageFormat::YUYV422), 
          "YUV420P" => Some(ImageFormat::YUV420P), 
          "YUVJ420P" => Some(ImageFormat::YUVJ420P), 
          "NV12" => Some(ImageFormat::NV12), 
          "BGR" => Some(ImageFormat::BGR), 
          "MPEG4" => Some(ImageFormat::MPEG4),
          _ => None
        };
      },
      "Width" => {
        if let StateValue::Int(v) = value {
          let mut lock = this.params.lock().unwrap();
          lock.dirty = true;
          lock.params.width = if v > 0 { Some(v as i32) } else { None };
        }
      },
      "Height" => {
        if let StateValue::Int(v) = value {
          let mut lock = this.params.lock().unwrap();
          lock.dirty = true;
          lock.params.height = if v > 0 { Some(v as i32) } else { None };
        }
      },
      "Quality" => {
        if let StateValue::Int(v) = value {
          let mut lock = this.params.lock().unwrap();
          lock.dirty = true;
          lock.params.quality = if v > 0 { Some(v as i32) } else { None };
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
          let signaler = borrow.signaler.clone();
          let notify = borrow.notify.clone();
          borrow.data_connection = Some(node.subscribe_multithreaded(move |node| {
            let mut converter = converter.lock().unwrap();
            {
              let mut params = params.lock().unwrap();
              if params.dirty {
                converter.reconfigure(params.params);
                params.dirty = false;
              }
            }

            let data = node.data();
            if !converter.needs_conversion(&data) {
              let _ = signaler.ready_this_thread(&data);
            } else {
              converter.push(&data);
              notify.notify_one();
            }
          }));
        }));
        rc.borrow_mut().source_connection = temp;
      }
      _ => {}
    }
  }

  async fn converter_task(converter: Arc<Mutex<Converter>>, signaler: Arc<OffMainSignaler>, notify: Arc<Notify>, dropping: Arc<AtomicBool>) {
    loop {
      if dropping.load(Ordering::SeqCst) {
        return;
      }
      {
        let mut converter = converter.lock().unwrap();
        while let Some(image) = converter.pull() {
          let _ = signaler.ready(&*image);
        }
      }
      notify.notified().await;
    }
  }
}

impl Node for ImageConverterNode {
  fn new(api: ThalamusAPI, node_token: NodeToken, state: State, _token: MainThreadToken) -> Rc<RefCell<Self>> {
    let signaler = OffMainSignaler::new(api, node_token);
    signaler.unblock();
    let params = ParamsHolder {
      params: ConverterParams { 
        format: None, 
        width: None, 
        height: None, 
        quality: None,
      },
      dirty: true,
    };
    let result = Rc::new(RefCell::new(ImageConverterNode {
      params: Arc::new(Mutex::new(params.clone())),
      converter: Arc::new(Mutex::new(Converter::new(api.thread_safe(), params.params))),
      api,
      state_connection: None,
      source_connection: None,
      data_connection: None,
      signaler,
      notify: Arc::new(Notify::new()),
      dropping: Arc::new(AtomicBool::new(false))
    }));

    let change_ref = Rc::downgrade(&result);
    let state_callback =
      move |s, a, k, v| {
        if let Some(lock) = change_ref.upgrade() {
          ImageConverterNode::on_state(lock, s, a, k, v);
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
      api.tokio().as_ref().unwrap().spawn(ImageConverterNode::converter_task(converter, signaler, notify, dropping));
    }
    result
  }

  fn predrop(&self, token: crate::api::PredropToken) {
    self.dropping.store(true, Ordering::SeqCst);
    self.signaler.predrop(token);
    self.notify.notify_one();
  }
}
