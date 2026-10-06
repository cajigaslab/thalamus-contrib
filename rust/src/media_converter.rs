//! Converts both halves of node data: images with an image Converter and
//! analog data with an AudioConverter.

use crate::api::{NodeData, ThalamusAPIThreadSafe};
use crate::audio_converter::{AudioConverter, AudioConverterParams};
use crate::image_converter::{Converter, ConverterParams};

#[derive(Debug, Clone, Copy)]
pub struct MediaConverterParams {
  pub image: ConverterParams,
  pub audio: AudioConverterParams,
}

pub struct MediaConverter {
  image: Converter,
  audio: AudioConverter,
  params: MediaConverterParams,
}

impl MediaConverter {
  pub fn new(api: ThalamusAPIThreadSafe, params: MediaConverterParams) -> MediaConverter {
    MediaConverter {
      image: Converter::new(api, params.image),
      audio: AudioConverter::new(api, params.audio),
      params,
    }
  }

  /// Reconfigures only the converters whose parameters changed, so e.g. an
  /// image setting doesn't reset the audio encoder.
  pub fn reconfigure(&mut self, params: MediaConverterParams) {
    if params.image != self.params.image {
      self.image.reconfigure(params.image);
    }
    if params.audio != self.params.audio {
      self.audio.reconfigure(params.audio);
    }
    self.params = params;
  }

  /// Queues whichever of `data`'s image and analog data needs converting.
  pub fn push(&mut self, data: &dyn NodeData) {
    self.image.push(data);
    // The audio converter picks its input channels once, so it starts over
    // when they change.
    if data.analog().is_some_and(|analog| analog.channels_changed()) {
      self.audio.channels_changed();
    }
    self.audio.push(data);
  }

  /// The next converted output: audio first, since it's owned (returning a
  /// borrowed image and then falling back to audio doesn't borrow check).
  pub fn pull<'a>(&'a mut self) -> Option<Box<dyn NodeData + 'a>> {
    if let Some(audio) = self.audio.pull() {
      return Some(Box::new(audio));
    }
    self.image.pull()
  }
}
