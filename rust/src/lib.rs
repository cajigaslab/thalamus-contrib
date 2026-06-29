use std::ptr;

mod ffi;
mod wakers;
pub mod api;
mod joystick_node;
mod serialtouchscreen_node;
mod aruco_node;
mod thorcam_node;

use api::{
  ThalamusNode,
  ThalamusAPIRaw,
  WrappableNode,
  ThalamusNodeFactory,
};
use joystick_node::JoystickNode;
use serialtouchscreen_node::SerialTouchscreenNode;
use aruco_node::ArucoNode;
use thorcam_node::ThorcamNode;

export_nodes!(
  ("JOYSTICK", JoystickNode),
  ("SERIAL_TOUCH_SCREEN", SerialTouchscreenNode),
  ("ARUCO", ArucoNode),
  ("THORCAM", ThorcamNode)
);

