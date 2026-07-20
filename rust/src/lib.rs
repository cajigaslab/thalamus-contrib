use std::ptr;

mod ffi;
mod wakers;
pub mod api;
mod thorcam_node;
mod ffmpeg_devices;
mod webcam_node;

use thorcam_node::ThorcamNode;
use webcam_node::WebcamNode;

export_nodes!(
  ("THORCAM", ThorcamNode),
  ("WEBCAM", WebcamNode)
);

