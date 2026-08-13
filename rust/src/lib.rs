use std::ptr;

mod ffi;
mod wakers;
pub mod api;
mod thorcam_node;
mod ffmpeg_devices;
mod webcam_node;
mod rtmps_publisher;
mod rtmps_node;
mod angular_scaling_node;

use thorcam_node::ThorcamNode;
use webcam_node::WebcamNode;
use rtmps_node::RtmpsNode;
use angular_scaling_node::AngularScalingNode;

export_nodes!(
  ("THORCAM", ThorcamNode),
  ("WEBCAM", WebcamNode),
  ("RTMPS", RtmpsNode),
  ("ANGULAR_SCALING", AngularScalingNode)
);

