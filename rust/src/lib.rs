// Node::new() is declared as `-> impl IntoNodeHandle` so a node can opt into
// returning Arc<Self>/Rc<Self> instead of Self; node impls are intentionally
// allowed to name their concrete return type (Self, Arc<Self>, Rc<Self>)
// instead of writing `impl IntoNodeHandle` themselves, so direct/test calls
// to a node's new() see the concrete type rather than the opaque one.
#![allow(refining_impl_trait)]

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
use rtmps_node::RtmpsNode;
use sleeve_node::SleeveNode;
use angular_scaling_node::AngularScalingNode;
use webcam_node::WebcamNode;

export_nodes!(
  ("THORCAM", ThorcamNode),
  ("RTMPS", RtmpsNode),
  ("SLEEVE", SleeveNode),
  ("ANGULAR_SCALING", AngularScalingNode),
  ("WEBCAM", WebcamNode)
);

