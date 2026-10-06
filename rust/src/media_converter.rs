//! Converts both halves of node data: images with an image Converter and
//! analog data with an AudioConverter.

use std::sync::Mutex;

use crate::api::{NodeData, ThalamusAPIThreadSafe};
use crate::audio_converter::{AudioConverter, AudioConverterParams};
use crate::image_converter::{ConverterParams, ImageConverter};

#[derive(Debug, Clone, Copy)]
pub struct MediaConverterParams {
  pub image: ConverterParams,
  pub audio: AudioConverterParams,
}

/// Shared between the thread that pushes and the task that pulls. Both
/// converters do their own locking, so pushing doesn't wait for pulled data
/// to be decoded or encoded.
pub struct MediaConverter {
  image: ImageConverter,
  audio: AudioConverter,
  params: Mutex<MediaConverterParams>,
}

impl MediaConverter {
  pub fn new(api: ThalamusAPIThreadSafe, params: MediaConverterParams) -> MediaConverter {
    MediaConverter {
      image: ImageConverter::new(api, params.image),
      audio: AudioConverter::new(api, params.audio),
      params: Mutex::new(params),
    }
  }

  /// Reconfigures only the converters whose parameters changed, so e.g. an
  /// image setting doesn't reset the audio encoder.
  pub fn reconfigure(&self, params: MediaConverterParams) {
    let mut current = self.params.lock().unwrap();
    if params.image != current.image {
      self.image.reconfigure(params.image);
    }
    if params.audio != current.audio {
      self.audio.reconfigure(params.audio);
    }
    *current = params;
  }

  /// Queues whichever of `data`'s image and analog data needs converting.
  pub fn push(&self, data: &dyn NodeData) {
    self.image.push(data);
    self.audio.push(data);
  }

  /// The next converted message: audio if there is any, otherwise an image.
  /// Neither converter's pull holds up pushes.
  pub fn pull(&self) -> Option<Box<dyn NodeData + '_>> {
    if let Some(output) = self.audio.pull() {
      return Some(Box::new(output));
    }
    self.image.pull()
  }
}
