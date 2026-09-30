// Node::new() is declared as `-> impl IntoNodeHandle` so a node can opt into
// returning Arc<Self>/Rc<Self> instead of Self; node impls are intentionally
// allowed to name their concrete return type (Self, Arc<Self>, Rc<Self>)
// instead of writing `impl IntoNodeHandle` themselves, so direct/test calls
// to a node's new() see the concrete type rather than the opaque one.
#![allow(refining_impl_trait)]

use std::ptr;

mod angular_scaling_node;
pub mod api;
mod audio_converter;
mod block;
mod bluetooth;
mod ffi;
mod ffmpeg_devices;
mod image_converter;
mod image_converter_node;
mod image_viewer;
mod imgui_platform;
mod imgui_window;
mod media_converter;
mod mic_node;
mod rtmps_node;
mod rtmps_publisher;
mod sleeve_node;
mod thorcam_node;
mod wakers;
mod webcam_node;

use angular_scaling_node::AngularScalingNode;
use image_converter_node::ImageConverterNode;
use mic_node::MicNode;
use rtmps_node::RtmpsNode;
use sleeve_node::SleeveNode;
use thorcam_node::ThorcamNode;
use webcam_node::WebcamNode;

export_nodes!(
  ("THORCAM", ThorcamNode),
  ("RTMPS", RtmpsNode),
  ("SLEEVE", SleeveNode),
  ("ANGULAR_SCALING", AngularScalingNode),
  ("WEBCAM", WebcamNode),
  ("IMAGE_CONVERTER", ImageConverterNode),
  ("MIC", MicNode)
);
