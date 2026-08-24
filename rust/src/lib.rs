use std::ptr;

mod ffi;
mod wakers;
pub mod api;
mod imgui_platform;
mod imgui_window;
mod image_viewer;
mod thorcam_node;
mod ffmpeg_devices;
mod webcam_node;
mod rtmps_publisher;
mod rtmps_node;
mod sleeve_node;
mod angular_scaling_node;
mod block;
mod bluetooth;

use thorcam_node::ThorcamNode;
use webcam_node::WebcamNode;
use rtmps_node::RtmpsNode;
use sleeve_node::SleeveNode;
use angular_scaling_node::AngularScalingNode;

export_nodes!(
  ("THORCAM", ThorcamNode),
  ("WEBCAM", WebcamNode),
  ("RTMPS", RtmpsNode),
  ("SLEEVE", SleeveNode),
  ("ANGULAR_SCALING", AngularScalingNode)
);

