use std::ptr;

mod ffi;
mod wakers;
pub mod api;
mod thorcam_node;
mod ffmpeg_devices;
mod webcam_node;
mod rtmps_publisher;
mod rtmps_node;
mod sleeve_node;

use thorcam_node::ThorcamNode;
use webcam_node::WebcamNode;
use rtmps_node::RtmpsNode;
use rtmps_node::RtmpsNode;
use sleeve_node::SleeveNode;

export_nodes!(
  ("THORCAM", ThorcamNode),
  ("WEBCAM", WebcamNode),
  ("RTMPS", RtmpsNode),
  ("SLEEVE", SleeveNode)
);

