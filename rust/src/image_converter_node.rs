use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use crate::api::{
  ImageFormat,
  MainThreadToken, Node, NodeConsts, NodeSelector, NodeToken, OffMainSignaler, OnDrop, State, StateAction, StateValue, THALAMUS_MODALITY_IMAGE, ThalamusAPI,
};
use crate::image_converter::{Converter, ConverterParams};

pub struct ImageConverterNode {
  api: ThalamusAPI,
  params: Arc<Mutex<(ConverterParams, bool)>>,
  state_connection: Option<OnDrop>,
  source_connection: Option<OnDrop>,
  data_connection: Option<OnDrop>,
  signaler: Arc<OffMainSignaler>,
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

        let mut params = this.params.lock().unwrap();
        let format = match v.to_uppercase().as_str() {
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
        *params = (ConverterParams { format, ..params.0}, true);
        //*this.converter.lock().unwrap() = Converter::new(this.api.thread_safe(), this.params);
      },
      "Width" => {
        if let StateValue::Int(v) = value {
          let mut params = this.params.lock().unwrap();
          let width = if v > 0 { Some(v as i32) } else { None };
          *params = (ConverterParams { width, ..params.0}, true);
        }
      },
      "Height" => {
        if let StateValue::Int(v) = value {
          let mut params = this.params.lock().unwrap();
          let height = if v > 0 { Some(v as i32) } else { None };
          *params = (ConverterParams { height, ..params.0}, true);
        }
      },
      "Quality" => {
        if let StateValue::Int(v) = value {
          let mut params = this.params.lock().unwrap();
          let quality = if v > 0 { Some(v as i32) } else { None };
          *params = (ConverterParams { quality, ..params.0}, true);
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

          let api = borrow.api.thread_safe();
          let params = borrow.params.clone();
          let mut params_lock = borrow.params.lock().unwrap();
          let mut converter = Converter::new(api, params_lock.0);
          params_lock.1 = false;
          drop(params_lock);

          let signaler = borrow.signaler.clone();
          borrow.data_connection = Some(node.subscribe_multithreaded(move |node| {
            let data = node.data();

            {
              let mut params_lock = params.lock().unwrap();
              if params_lock.1 {
                converter = Converter::new(api, params_lock.0);
                params_lock.1 = false;
              }
            }

            let mut result = converter.push(&data);
            while let Some(image) = result.pull() {
              let _ = signaler.ready_this_thread(image.as_ref());
            }
          }));
        }));
        rc.borrow_mut().source_connection = temp;
      }
      _ => {}
    }
  }
}

impl Node for ImageConverterNode {
  fn new(api: ThalamusAPI, node_token: NodeToken, state: State, _token: MainThreadToken) -> Rc<RefCell<Self>> {
    let signaler = OffMainSignaler::new(api, node_token);
    signaler.unblock();
    let params = ConverterParams { 
      format: None, 
      width: None, 
      height: None, 
      quality: None,
    };
    let result = Rc::new(RefCell::new(ImageConverterNode {
      params: Arc::new(Mutex::new((params, true))),
      api,
      state_connection: None,
      source_connection: None,
      data_connection: None,
      signaler,
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

    result
  }

  fn predrop(&self, token: crate::api::PredropToken) {
    self.signaler.predrop(token);
  }
}
