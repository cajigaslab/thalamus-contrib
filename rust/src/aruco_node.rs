use std::cell::{Ref, RefCell};
use std::collections::HashMap;
use std::ops::Deref;
use std::rc::{Rc, Weak};
use std::time::Duration;
use serde::Serialize;

use crate::api::{
    ImageFormat, ImageNode, MainThreadOnly, MainThreadToken, Node, NodeSelector, OnDrop, Request,
    Json, State, StateAction, StateKey, TaskScope, ThalamusAPI, StateValue
};

#[derive(Serialize, Clone)]
struct Board {
  marker_size: f64,
  marker_separation: f64,
  rows: i32,
  columns: i32,
  ids: Vec<i32>,
}

struct DistortionConfig {
  distortion_config: Option<State>,
  distortion_coefficients: Vec<f64>,
  camera_matrix: Vec<Vec<f64>>,
  connection: Option<OnDrop>,
  image_connection: Option<OnDrop>,
}

impl DistortionConfig {
  fn print_params(&self) {
    let coef_text = serde_json::to_string_pretty(&self.distortion_coefficients).unwrap();
    let matrix_text = serde_json::to_string_pretty(&self.camera_matrix).unwrap();
    println!("DISTORTION PARAMETERS");
    println!("{}", coef_text);
    println!("{}", matrix_text);
  }

  fn on_change(&mut self, _source: State, _action: StateAction, _key: StateValue, _value: StateValue) {
    println!("DistortionConfig::on_change {:?} {:?} {:?}", _source, _key, _value);

    let distortion_config = match self.distortion_config.clone() {
      Some(s) => s,
      None => return,
    };
    if let Some(StateValue::List(val)) = distortion_config.get(StateKey::String("Distortion Coefficients".to_string())) {
      self.distortion_coefficients = val.into_iter().filter_map(|e| {
        if let StateValue::Float(i) = e.val { Some(i) } else { None }
      }).collect();
    }
    if let Some(StateValue::List(val)) = distortion_config.get(StateKey::String("Camera Matrix".to_string())) {
      self.camera_matrix = val.into_iter().filter_map(|e| {
        if let StateValue::List(row) = e.val {
          let result = row.into_iter().filter_map(|e2| {
            if let StateValue::Float(i) = e2.val { Some(i) } else { None }
          }).collect();
          Some(result)
        } else {
          None
        }
      }).collect();
    }

    self.print_params();
  }
}

struct ArucoNodeInner {
  state: State,
  state_connection: Option<OnDrop>,
  api: ThalamusAPI,
  token: MainThreadToken,
  time: Duration,
  task: Option<TaskScope>,
  frame: Vec<u8>,
  boards: Option<State>,
  board_map: HashMap<State, Board>,
  dictionary: String,
  get_node: Option<OnDrop>,
  distortion_config: Rc<RefCell<DistortionConfig>>,
  weak_self: Weak<RefCell<ArucoNodeInner>>,
}

pub struct ArucoNode {
    inner: Rc<RefCell<ArucoNodeInner>>,
}

impl ArucoNodeInner {
  fn print_boards(&self) {
    let text = serde_json::to_string_pretty(&self.board_map.values().collect::<Vec<_>>()).unwrap();
    println!("BOARD RESULT");
    println!("{}", text);
  }

  fn update_board(&mut self, board_state: &State, delete: bool) {
    if delete {
      self.board_map.remove(board_state);
      self.print_boards();
      return;
    }
    if !self.board_map.contains_key(board_state) {
      self.board_map.insert(board_state.clone(), Board {
        marker_size: 0.0,
        marker_separation: 0.0,
        rows: 0,
        columns: 0,
        ids: vec![],
      });
    }
    let board = self.board_map.get_mut(board_state).unwrap();

    if let Some(StateValue::Float(val)) = board_state.get(StateKey::String("Marker Size".to_string())) {
      board.marker_size = val;
    }
    if let Some(StateValue::Float(val)) = board_state.get(StateKey::String("Marker Separation".to_string())) {
      board.marker_separation = val;
    }
    if let Some(StateValue::Int(val)) = board_state.get(StateKey::String("Rows".to_string())) {
      board.rows = val as i32;
    }
    if let Some(StateValue::Int(val)) = board_state.get(StateKey::String("Columns".to_string())) {
      board.columns = val as i32;
    }
    if let Some(StateValue::List(val)) = board_state.get(StateKey::String("ids".to_string())) {
      board.ids = val.into_iter().filter_map(|e| {
        if let StateValue::Int(i) = e.val { Some(i as i32) } else { None }
      }).collect();
    }

    self.print_boards();
  }


  fn on_change(&mut self, source: State, action: StateAction, key: StateValue, value: StateValue) {
    println!("BASE ArucoNodeInner::on_change2 {:?} {:?} {:?}", source, key, value);

    if source == self.state {
      println!("ROOT ArucoNodeInner::on_change2 {:?} {:?}", key, value);
      match key {
        StateValue::String(s) => {
          if s == "Boards" {
            if let StateValue::List(val) = value {
              self.boards = Some(val);
              if let Some(boards) = self.boards.clone() {
                boards.recap_with(|s, a, k, v| {
                  self.on_change(s, a, k, v);
                });
              }
            }
          } else if s == "Dictionary" {
            if let StateValue::String(val) = value {
              self.dictionary = val;
            }
          } else if s == "Source" {
            if let StateValue::String(val) = value {
              let dis_config = self.distortion_config.clone();
              let weak = self.weak_self.clone();
              let api = self.api;
              let token = self.token;
              self.get_node = Some(self.api.get_node(NodeSelector::Name(val), move |node| {
                let dis_state = node.state();
                let dis_config2 = dis_config.clone();
                let state_callback = move |s, a, k, v| {
                  dis_config2.borrow_mut().on_change(s, a, k, v);
                };
                dis_config.borrow_mut().distortion_config = Some(dis_state.clone());
                dis_state.recap_with(&state_callback);
                let connection = dis_state.connect(state_callback);

                let dis_config3 = dis_config.clone();
                let weak2 = weak.clone();
                let image_connection = node.subscribe(move |frame_node| {
                  let dc = dis_config3.borrow();
                  let coefficients = dc.distortion_coefficients.clone();
                  let matrix = dc.camera_matrix.clone();
                  drop(dc);

                  let boards = weak2.upgrade().map(|inner| {
                    inner.borrow().board_map.values().cloned().collect::<Vec<_>>()
                  }).unwrap_or_default();

                  let frame_data = frame_node.image()
                    .map(|img| img.plane(0).to_vec())
                    .unwrap_or_default();
                  let time = frame_node.time();

                  let weak_send = MainThreadOnly::new(weak2.clone(), token);

                  api.post_to_threadpool(move || {
                    let coef_text = serde_json::to_string_pretty(&coefficients).unwrap();
                    let matrix_text = serde_json::to_string_pretty(&matrix).unwrap();
                    let boards_text = serde_json::to_string_pretty(&boards).unwrap();
                    println!("DISTORTION PARAMETERS");
                    println!("{}", coef_text);
                    println!("{}", matrix_text);
                    println!("BOARDS");
                    println!("{}", boards_text);

                    // Placeholder: actual aruco computation on frame_data goes here.
                    let result_frame = frame_data;
                    let result_time = time;

                    api.post_to_main(move |token| {
                      if let Some(inner_rc) = weak_send.get(token).upgrade() {
                        let api = {
                          let mut inner = inner_rc.borrow_mut();
                          inner.frame = result_frame;
                          inner.time = result_time;
                          inner.api
                        };
                        api.ready(token);
                      }
                    });
                  });
                });

                let mut dc = dis_config.borrow_mut();
                dc.connection = Some(connection);
                dc.image_connection = Some(image_connection);
              }));
            }
          }
        }
        _ => {}
      }
    } else if self.boards.as_ref() == Some(&source) {
      println!("BOARDS ArucoNodeInner::on_change2 {:?} {:?}", key, value);
      if let StateValue::Dict(board_state) = value {
        self.update_board(&board_state, action == StateAction::Delete);
      }
    } else if self.boards == source.parent() {
      println!("BOARD ArucoNodeInner::on_change2 {:?} {:?}", key, value);
      self.update_board(&source, false);
    } else if let Some(parent) = source.parent() {
      if self.boards == parent.parent() {
        println!("ID ArucoNodeInner::on_change2 {:?} {:?}", key, value);
        self.update_board(&parent, false);
      }
    }
  }
}

impl ImageNode for ArucoNode {
    fn plane(&self, _channel: i32) -> impl Deref<Target = [u8]> {
      Ref::map(self.inner.borrow(), |v| v.frame.as_slice())
    }

    fn num_planes(&self) -> u64 {
        1
    }

    fn format(&self) -> ImageFormat {
        ImageFormat::Gray
    }

    fn width(&self) -> u64 {
        0
    }

    fn height(&self) -> u64 {
        0
    }

    fn frame_interval(&self) -> Duration {
        Duration::from_millis(16)
    }
}

impl Node for ArucoNode {
  fn time(&self) -> Duration {
    self.inner.borrow().time
  }

  fn process(&self, handle: Request, _: Json) {
    handle.respond(&Json::from_string(self.inner.borrow().api, "{}"));
  }

  fn new(api: ThalamusAPI, state: State, token: MainThreadToken) -> Self {
    let inner = Rc::new(RefCell::new(ArucoNodeInner {
      state: state.clone(),
      state_connection: None,
      api,
      token,
      time: Duration::from_millis(0),
      task: None,
      frame: vec![0],
      boards: None,
      board_map: HashMap::new(),
      dictionary: String::new(),
      get_node: None,
      distortion_config: Rc::new(RefCell::new(DistortionConfig {
        distortion_config: None,
        distortion_coefficients: vec![],
        camera_matrix: vec![],
        connection: None,
        image_connection: None,
      })),
      weak_self: Weak::new(),
    }));

    inner.borrow_mut().weak_self = Rc::downgrade(&inner);

    let change_ref = Rc::downgrade(&inner);
    let callback = move |source, action, key, value| {
      change_ref.upgrade().map(|val| {
        val.borrow_mut().on_change(source, action, key, value);
      });
    };

    {
      let mut inner_mut = inner.borrow_mut();
      inner_mut.state_connection = Some(inner_mut.state.connect(callback));
    }
    state.recap_with(|s, a, k, v| {
      inner.borrow_mut().on_change(s, a, k, v);
    });

    ArucoNode { inner }
  }
}

impl Drop for ArucoNode {
  fn drop(&mut self) {
    self.inner.borrow_mut().task = None;
  }
}
