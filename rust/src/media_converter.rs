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
      audio: AudioConverter::new(params.audio),
      params,
    }
  }

  /// Reconfigures only the converters whose parameters changed, so e.g. an
  /// image setting doesn't reset the audio encoder.
  pub fn reconfigure(&mut self, params: MediaConverterParams) {
    if !same_image_params(&params.image, &self.params.image) {
      self.image.reconfigure(params.image);
    }
    self.audio.reconfigure(params.audio);
    self.params = params;
  }

  pub fn needs_image_conversion(&self, data: &dyn NodeData) -> bool {
    self.image.needs_conversion(data)
  }

  pub fn needs_audio_conversion(&self, data: &dyn NodeData) -> bool {
    self.audio.needs_conversion(data)
  }

  /// Queues whichever of `data`'s image and analog data needs converting.
  pub fn push(&mut self, data: &dyn NodeData) {
    if self.image.needs_conversion(data) {
      self.image.push(data);
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

/// ConverterParams doesn't implement PartialEq, and the image converter
/// isn't changed here.
fn same_image_params(a: &ConverterParams, b: &ConverterParams) -> bool {
  a.format == b.format && a.width == b.width && a.height == b.height && a.quality == b.quality
}
