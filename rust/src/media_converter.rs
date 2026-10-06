//! Converts both halves of node data: images with an image Converter and
//! analog data with an AudioConverter.

use std::sync::{Mutex, MutexGuard};

use crate::api::{NodeData, ThalamusAPIThreadSafe};
use crate::audio_converter::{AudioConverter, AudioConverterParams};
use crate::image_converter::{Converter, ConverterParams};

#[derive(Debug, Clone, Copy)]
pub struct MediaConverterParams {
  pub image: ConverterParams,
  pub audio: AudioConverterParams,
}

/// Shared between the thread that pushes and the task that pulls. The audio
/// converter does its own locking, so pushing audio doesn't wait for pulled
/// audio to be decoded or encoded; the image converter is behind a mutex.
pub struct MediaConverter {
  image: Mutex<Converter>,
  audio: AudioConverter,
  params: Mutex<MediaConverterParams>,
}

impl MediaConverter {
  pub fn new(api: ThalamusAPIThreadSafe, params: MediaConverterParams) -> MediaConverter {
    MediaConverter {
      image: Mutex::new(Converter::new(api, params.image)),
      audio: AudioConverter::new(api, params.audio),
      params: Mutex::new(params),
    }
  }

  /// Reconfigures only the converters whose parameters changed, so e.g. an
  /// image setting doesn't reset the audio encoder.
  pub fn reconfigure(&self, params: MediaConverterParams) {
    let mut current = self.params.lock().unwrap();
    if params.image != current.image {
      self.image.lock().unwrap().reconfigure(params.image);
    }
    if params.audio != current.audio {
      self.audio.reconfigure(params.audio);
    }
    *current = params;
  }

  /// Queues whichever of `data`'s image and analog data needs converting.
  pub fn push(&self, data: &dyn NodeData) {
    self.image.lock().unwrap().push(data);
    self.audio.push(data);
  }

  /// Converted audio comes from AudioConverter::pull.
  pub fn audio(&self) -> &AudioConverter {
    &self.audio
  }

  /// Converted images come from Converter::pull, while the guard is held.
  pub fn image(&self) -> MutexGuard<'_, Converter> {
    self.image.lock().unwrap()
  }
}
