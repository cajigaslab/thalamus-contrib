//! Converts both halves of node data: images with an image Converter and
//! analog data with an AudioConverter.

use std::sync::Mutex;
use std::time::Duration;

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

  /// The next converted message, audio if there is any, otherwise an image,
  /// and the time of the input message that produced it. Outputs are timed
  /// by when their data was acquired, which for encoded audio is earlier
  /// than that input. Neither converter's pull holds up pushes.
  pub fn pull(&self) -> Option<(Box<dyn NodeData + '_>, Duration)> {
    if let Some(output) = self.audio.pull() {
      let input_time = output.input_time();
      return Some((Box::new(output), input_time));
    }
    // Images keep their input's time.
    self.image.pull().map(|output| {
      let input_time = output.time();
      (output, input_time)
    })
  }
}
