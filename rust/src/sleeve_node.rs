use std::cell::{RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use crate::api::{Node, OnDrop, NodeToken, State, MainThreadToken, ThalamusAPI, Request, Json, StateAction, StateValue};

struct SleeveNodeInner {
    api:               ThalamusAPI,
    node_token:        NodeToken,
    state:             State,
    state_connection:  OnDrop,
    camera_thread:     Option<std::thread::JoinHandle<()>>,
    main_thread_token: MainThreadToken,
}

pub struct SleeveNode {
    inner: Arc<Mutex<SleeveNodeInner>>,
}

impl Node for SleeveNode {
  fn process(&self, handle: Request, _request: Json) {
    let api = self.inner.lock().unwrap().api;
    handle.respond(&Json::from_string(api, "null"));
  }

  fn new(api: ThalamusAPI, node_token: NodeToken, state: State, main_thread_token: MainThreadToken) -> Self {
    
    let inner = Arc::new_cyclic(|weak| {

      let state_callback =
        move |_source: State, _action: StateAction, key: StateValue, value: StateValue| {
          let lock = weak.upgrade();
          let StateValue::String(key_str) = key else {
            return;
          };
          match key_str.as_str() {
            "Running" => {
              if value == StateValue::Bool(true) {
                start_capture(&change_ref);
              } else {
                stop_capture(&change_ref, || {});
              }
            }
            _ => {}
          }
        };
      let state_connection = state.connect(state_callback);

      Mutex::new(SleeveNodeInner {
        api, node_token, state,
      })
    });

    SleeveNode {
      inner
    }
  }

}
