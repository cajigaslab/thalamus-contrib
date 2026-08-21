use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::api::{
  AnalogData, ImageData, Json, MainThreadToken, MocapData, Node, NodeData, NodeSelector,
  NodeToken, OnDrop, PredropToken, Request, State, StateAction, StateKey, StateValue,
  THALAMUS_MODALITY_ANALOG, ThalamusAPI,
};

fn state_get_string(state: &State, key: &str) -> Option<String> {
  match state.get(StateKey::String(key.to_string())) {
    Some(StateValue::String(s)) if !s.is_empty() => Some(s),
    _ => None,
  }
}

fn state_get_dict(state: &State, key: &str) -> Option<State> {
  match state.get(StateKey::String(key.to_string())) {
    Some(StateValue::Dict(d)) => Some(d),
    _ => None,
  }
}

fn state_get_list(state: &State, key: &str) -> Option<State> {
  match state.get(StateKey::String(key.to_string())) {
    Some(StateValue::List(l)) => Some(l),
    _ => None,
  }
}

fn state_get_f64(state: &State, key: &str) -> Option<f64> {
  match state.get(StateKey::String(key.to_string())) {
    Some(StateValue::Float(f)) => Some(f),
    Some(StateValue::Int(i)) => Some(i as f64),
    _ => None,
  }
}

/// One calibration pin from the "Angular Scaling" eye-tracking model: an eye
/// angle/screen rotation pair, plus the eye-magnitude -> screen-magnitude
/// notches used to interpolate scale at that angle. Mirrors `Pin` in
/// rust-executor's `src/eye_tracking.rs`, which itself mirrors
/// `AngularScalingConfig` in Thalamus's `task_controller/canvas.py`.
struct Pin {
  eye_angle: f64,
  rotation: f64,
  notches: Vec<(f64, f64)>,
}

/// The parsed "Angular Scaling" model: calibration pins plus the flat
/// fallback scale used when no pins are configured.
struct AngularScaling {
  pins: Vec<Pin>,
  scale_default: f64,
}

/// Walks up from `state` (this node's own config dict) to the root of the
/// whole Thalamus config tree. The eye calibration this node applies is read
/// from `['eye_scaling']` at that root -- the same location rust-executor
/// reads it from (see `eye_tracking::refresh_angular_scaling` in its
/// `src/eye_tracking.rs`) -- rather than from this node's own state.
fn root_state(state: &State) -> State {
  let mut root = state.clone();
  while let Some(parent) = root.parent() {
    root = parent;
  }
  root
}

/// Parses `eye_scaling['Models']['Angular Scaling']` out of the root config's
/// `['eye_scaling']` dict. Mirrors
/// `eye_tracking::refresh_angular_scaling` in rust-executor's
/// `src/eye_tracking.rs`.
fn parse_angular_scaling(eye_scaling: &State) -> AngularScaling {
  let model =
    state_get_dict(eye_scaling, "Models").and_then(|models| state_get_dict(&models, "Angular Scaling"));

  let Some(model) = model else {
    return AngularScaling { pins: Vec::new(), scale_default: 100.0 };
  };

  let scale_default = state_get_f64(&model, "Scale Default").unwrap_or(100.0);

  let mut pins = Vec::new();
  if let Some(pin_list) = state_get_list(&model, "Pins") {
    for entry in &pin_list {
      let StateValue::Dict(pin) = entry.val else { continue };

      let mut notches = vec![(0.0, 0.0)];
      if let Some(notch_list) = state_get_list(&pin, "Notches") {
        for notch_entry in &notch_list {
          let StateValue::Dict(notch) = notch_entry.val else { continue };
          notches.push((
            state_get_f64(&notch, "Eye").unwrap_or(0.0),
            state_get_f64(&notch, "Screen").unwrap_or(0.0),
          ));
        }
      }

      pins.push(Pin {
        eye_angle: state_get_f64(&pin, "Angle").unwrap_or(0.0),
        rotation: state_get_f64(&pin, "Rotation").unwrap_or(0.0),
        notches,
      });
    }
  }

  AngularScaling { pins, scale_default }
}

/// Piecewise-linear interpolation matching `numpy.interp`: `xp` must be
/// sorted ascending; queries outside `xp`'s range clamp to the nearest
/// endpoint's `fp` value.
fn interp(x: f64, xp: &[f64], fp: &[f64]) -> f64 {
  if x <= xp[0] {
    return fp[0];
  }
  if x >= *xp.last().unwrap() {
    return *fp.last().unwrap();
  }
  for i in 0..xp.len() - 1 {
    if x <= xp[i + 1] {
      let t = (x - xp[i]) / (xp[i + 1] - xp[i]);
      return fp[i] + t * (fp[i + 1] - fp[i]);
    }
  }
  *fp.last().unwrap()
}

/// Interpolates the eye-magnitude -> screen-magnitude scale for `mag` from
/// one pin's notches, extrapolating past the last notch using the slope of
/// its last two points (as `AngularScalingConfig.process` does) rather than
/// clamping to it.
fn notch_scale(notches: &[(f64, f64)], mag: f64) -> f64 {
  let (last_eye, last_screen) = notches[notches.len() - 1];
  if mag > last_eye {
    let (prev_eye, prev_screen) = notches[notches.len() - 2];
    (mag - last_eye) * (last_screen - prev_screen) / (last_eye - prev_eye) + last_screen
  } else {
    let eyes: Vec<f64> = notches.iter().map(|n| n.0).collect();
    let screens: Vec<f64> = notches.iter().map(|n| n.1).collect();
    interp(mag, &eyes, &screens)
  }
}

/// Ports `AngularScalingConfig.process` (see `task_controller/canvas.py` in
/// the Thalamus repo, and `eye_tracking::angular_scaling_process` in
/// rust-executor's `src/eye_tracking.rs`): converts a raw `(x, y)` gaze
/// reading into a point relative to screen center. With no calibration pins
/// configured, falls back to a flat `scale_default` multiply.
fn angular_scaling_process(x: f64, y: f64, pins: &[Pin], scale_default: f64) -> (f64, f64) {
  if pins.is_empty() {
    return (scale_default * x, scale_default * y);
  }

  let two_pi = 2.0 * std::f64::consts::PI;
  let mut val = y.atan2(x);
  if val < 0.0 {
    val += two_pi;
  }

  let mag = (x * x + y * y).sqrt();
  if mag == 0.0 {
    return (0.0, 0.0);
  }

  let eye_angles: Vec<f64> = pins.iter().map(|p| p.eye_angle).collect();
  let screen_angles: Vec<f64> = pins.iter().map(|p| p.rotation).collect();
  // `bisect_left`: leftmost index at which `val` could be inserted to keep
  // `eye_angles` sorted.
  let i = eye_angles.partition_point(|&angle| angle < val);

  let (lower_i, upper_i, eye_angle_lower) = if i == pins.len() || i == 0 {
    if val > std::f64::consts::PI {
      val -= two_pi;
    }
    (pins.len() - 1, 0, eye_angles[pins.len() - 1] - two_pi)
  } else if pins.len() == 1 {
    (0, 0, eye_angles[0])
  } else {
    (i - 1, i, eye_angles[i - 1])
  };

  let lower_scale = notch_scale(&pins[lower_i].notches, mag);
  let upper_scale = notch_scale(&pins[upper_i].notches, mag);

  let scale = interp(
    val,
    &[eye_angle_lower, eye_angles[upper_i]],
    &[lower_scale, upper_scale],
  );
  let mut rotation = interp(
    val,
    &[eye_angle_lower, eye_angles[upper_i]],
    &[screen_angles[lower_i], screen_angles[upper_i]],
  );
  if rotation > std::f64::consts::PI {
    rotation -= two_pi;
  } else if rotation < -std::f64::consts::PI {
    rotation += two_pi;
  }

  let (cos, sin) = (rotation.cos(), rotation.sin());
  let new_x = scale * (x * cos - y * sin) / mag;
  let new_y = scale * (x * sin + y * cos) / mag;
  (new_x, new_y)
}

/// Latest sample of the named channel (whichever of `data`/`short_data`/
/// `int_data`/`ulong_data` the source uses), with `scale`/`offset` applied
/// only if the source reports its data as transformed -- mirrors
/// `analog::last_span_value` in rust-executor's `src/analog.rs`.
fn channel_last_value(analog: &dyn AnalogData, name: &str) -> Option<f64> {
  for channel in 0..analog.num_channels() {
    if analog.name(channel) != name {
      continue;
    }
    let raw = if analog.is_short_data() {
      *analog.short_data(channel).last()? as f64
    } else if analog.is_int_data() {
      *analog.int_data(channel).last()? as f64
    } else if analog.is_ulong_data() {
      *analog.ulong_data(channel).last()? as f64
    } else {
      *analog.data(channel).last()?
    };
    return Some(if analog.is_transformed() {
      raw * analog.scale(channel) + analog.offset(channel)
    } else {
      raw
    });
  }
  None
}

/// The single-sample analog output this node publishes each time its
/// `Source` produces a new gaze reading: the angular-scaling-corrected
/// `(X, Y)` point, relative to screen center.
struct ScaledGazeData {
  x: f64,
  y: f64,
  time: Duration,
}

impl NodeData for ScaledGazeData {
  fn time(&self) -> Duration {
    self.time
  }
  fn analog(&self) -> Option<&dyn AnalogData> {
    Some(self)
  }
  fn image(&self) -> Option<&dyn ImageData> {
    None
  }
  fn mocap(&self) -> Option<&dyn MocapData> {
    None
  }
}

impl AnalogData for ScaledGazeData {
  fn data(&self, channel: i32) -> &[f64] {
    match channel {
      0 => std::slice::from_ref(&self.x),
      1 => std::slice::from_ref(&self.y),
      _ => &[],
    }
  }
  fn num_channels(&self) -> i32 {
    2
  }
  fn sample_interval(&self, _channel: i32) -> Duration {
    Duration::ZERO
  }
  fn name(&self, channel: i32) -> &str {
    match channel {
      0 => "X",
      1 => "Y",
      _ => "",
    }
  }
}

struct Inner {
  api: ThalamusAPI,
  node_token: NodeToken,
  state: State,
  state_connection: Option<OnDrop>,
  // Watches the root config's `['eye_scaling']` subtree (if present at node
  // creation) so `angular_scaling` stays fresh as calibration changes.
  eye_scaling_connection: Option<OnDrop>,
  angular_scaling: Arc<Mutex<AngularScaling>>,
  // Connected to the node named by "Source"; re-fires (rewiring
  // data_connection below) if the node graph changes which node that name
  // resolves to. Mirrors RtmpsNode's source_connection/data_connection.
  source_connection: Option<OnDrop>,
  data_connection: Option<OnDrop>,
  // (Fixation X, Fixation Y): offset added to every emitted point, kept fresh
  // as those keys change in this node's own state.
  fixation: Arc<Mutex<(f64, f64)>>,
}

pub struct AngularScalingNode {
  inner: Rc<RefCell<Inner>>,
}

fn resolve_source(inner: &Rc<RefCell<Inner>>) {
  let (api, node_token, source, angular_scaling, fixation) = {
    let borrow = inner.borrow();
    (
      borrow.api,
      borrow.node_token.clone(),
      state_get_string(&borrow.state, "Source"),
      Arc::clone(&borrow.angular_scaling),
      Arc::clone(&borrow.fixation),
    )
  };

  {
    let mut borrow = inner.borrow_mut();
    borrow.source_connection = None; // stop resolving/re-resolving "Source"
    borrow.data_connection = None; // stop receiving samples from the old source
  }

  let Some(source) = source else {
    println!("AngularScalingNode: no Source set");
    return;
  };

  let mt_api = api.thread_safe();
  let inner_for_source = Rc::clone(inner);
  let source_connection = api.get_node(NodeSelector::Name(source), move |ext_node| {
    // A node now matches "Source" -- either the first resolution, or the
    // previously matched node was replaced. Rewire sample delivery.
    let node_token = node_token.clone();
    let angular_scaling = Arc::clone(&angular_scaling);
    let fixation = Arc::clone(&fixation);
    let data_connection = ext_node.subscribe_multithreaded(move |ext_node| {
      let data = ext_node.data();
      let Some(analog) = data.analog() else {
        return;
      };
      let (Some(x), Some(y)) = (
        channel_last_value(analog, "X"),
        channel_last_value(analog, "Y"),
      ) else {
        return;
      };

      // Thalamus flips Y before scaling -- see `eye_tracking::run` in
      // rust-executor's `src/eye_tracking.rs`, mirroring Canvas.on_ros_gaze.
      let (out_x, out_y) = {
        let cache = angular_scaling.lock().unwrap();
        angular_scaling_process(x, -y, &cache.pins, cache.scale_default)
      };
      let (fixation_x, fixation_y) = *fixation.lock().unwrap();

      let output = ScaledGazeData {
        x: out_x + fixation_x,
        y: out_y + fixation_y,
        time: data.time(),
      };
      // Ignore if the node was destroyed concurrently.
      let _ = mt_api.ready_this_thread(&output, &node_token);
    });
    inner_for_source.borrow_mut().data_connection = Some(data_connection);
  });

  inner.borrow_mut().source_connection = Some(source_connection);
}

impl Node for AngularScalingNode {
  fn modalities(&self) -> u32 {
    THALAMUS_MODALITY_ANALOG
  }

  fn process(&self, handle: Request, _request: Json) {
    let api = self.inner.borrow().api;
    handle.respond(&Json::from_string(api, "null"));
  }

  fn new(api: ThalamusAPI, node_token: NodeToken, state: State, _token: MainThreadToken) -> Self {
    let eye_scaling_state = state_get_dict(&root_state(&state), "eye_scaling");
    let angular_scaling = Arc::new(Mutex::new(match &eye_scaling_state {
      Some(eye_scaling_state) => parse_angular_scaling(eye_scaling_state),
      None => AngularScaling { pins: Vec::new(), scale_default: 100.0 },
    }));

    let inner = Rc::new(RefCell::new(Inner {
      api,
      node_token,
      state: state.clone(),
      state_connection: None,
      eye_scaling_connection: None,
      angular_scaling: Arc::clone(&angular_scaling),
      source_connection: None,
      data_connection: None,
      fixation: Arc::new(Mutex::new((0.0, 0.0))),
    }));

    if let Some(eye_scaling_state) = eye_scaling_state {
      let refresh_ref = Arc::clone(&angular_scaling);
      let eye_scaling_for_refresh = eye_scaling_state.clone();
      let eye_scaling_connection = eye_scaling_state.connect(move |_source, _action, _key, _value| {
        *refresh_ref.lock().unwrap() = parse_angular_scaling(&eye_scaling_for_refresh);
      });
      inner.borrow_mut().eye_scaling_connection = Some(eye_scaling_connection);
    }

    let change_ref = Rc::clone(&inner);
    let state_callback =
      move |_source: State, _action: StateAction, key: StateValue, _value: StateValue| {
        let StateValue::String(key_str) = key else {
          return;
        };
        if key_str == "Source" {
          resolve_source(&change_ref);
        } else if key_str == "Fixation X" || key_str == "Fixation Y" {
          let borrow = change_ref.borrow();
          let mut fixation = borrow.fixation.lock().unwrap();
          fixation.0 = state_get_f64(&borrow.state, "Fixation X").unwrap_or(0.0);
          fixation.1 = state_get_f64(&borrow.state, "Fixation Y").unwrap_or(0.0);
        }
      };

    inner.borrow_mut().state_connection = Some(state.connect(state_callback));
    state.recap();

    AngularScalingNode { inner }
  }

  fn predrop(&self, token: PredropToken) {
    // Stop reacting to the source node before signaling ready, so no more
    // samples arrive while this node is being torn down.
    let mut borrow = self.inner.borrow_mut();
    borrow.source_connection = None;
    borrow.data_connection = None;
    drop(borrow);
    token.ready();
  }
}
