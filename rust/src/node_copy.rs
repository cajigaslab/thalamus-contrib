//! Owned copies of node data, for handing data received in a subscription
//! callback to another thread or task: the original is only valid during
//! the callback.

use std::time::Duration;

use crate::api::{AnalogData, AnalogEncoding, AnalogFormat, ImageData, ImageFormat, NodeData};

/// One channel's samples, in the type its format stores them as.
enum ChannelSamples {
  Double(Vec<f64>),
  Short(Vec<i16>),
  Int(Vec<i32>),
  ULong(Vec<u64>),
  /// Encoded channels' samples are in the buffer.
  Encoded,
}

struct ChannelCopy {
  samples: ChannelSamples,
  sample_interval: Duration,
  name: String,
  scale: f64,
  offset: f64,
}

/// A copy of an AnalogData's channels, encoded buffer and flags.
pub struct AnalogDataCopy {
  channels: Vec<ChannelCopy>,
  buffer: Vec<u8>,
  encoding: AnalogEncoding,
  encoded_count: u64,
  channels_changed: bool,
  is_short_data: bool,
  is_int_data: bool,
  is_ulong_data: bool,
  is_transformed: bool,
}

impl AnalogDataCopy {
  pub fn new(original: &dyn AnalogData) -> AnalogDataCopy {
    let channels = (0..original.num_channels()).map(|channel| {
      let samples = match original.analog_format(channel) {
        AnalogFormat::Double => ChannelSamples::Double(original.data(channel).to_vec()),
        AnalogFormat::Short => ChannelSamples::Short(original.short_data(channel).to_vec()),
        AnalogFormat::Int => ChannelSamples::Int(original.int_data(channel).to_vec()),
        AnalogFormat::ULong => ChannelSamples::ULong(original.ulong_data(channel).to_vec()),
        AnalogFormat::Encoded => ChannelSamples::Encoded,
      };
      ChannelCopy {
        samples,
        sample_interval: original.sample_interval(channel),
        name: original.name(channel).to_string(),
        scale: original.scale(channel),
        offset: original.offset(channel),
      }
    }).collect();
    AnalogDataCopy {
      channels,
      buffer: original.buffer().to_vec(),
      encoding: original.encoding(),
      encoded_count: original.encoded_count(),
      channels_changed: original.channels_changed(),
      is_short_data: original.is_short_data(),
      is_int_data: original.is_int_data(),
      is_ulong_data: original.is_ulong_data(),
      is_transformed: original.is_transformed(),
    }
  }

  fn samples(&self, channel: i32) -> Option<&ChannelSamples> {
    self.channels.get(usize::try_from(channel).ok()?).map(|c| &c.samples)
  }
}

impl AnalogData for AnalogDataCopy {
  fn data(&self, channel: i32) -> &[f64] {
    match self.samples(channel) {
      Some(ChannelSamples::Double(samples)) => samples,
      _ => &[],
    }
  }

  fn short_data(&self, channel: i32) -> &[i16] {
    match self.samples(channel) {
      Some(ChannelSamples::Short(samples)) => samples,
      _ => &[],
    }
  }

  fn int_data(&self, channel: i32) -> &[i32] {
    match self.samples(channel) {
      Some(ChannelSamples::Int(samples)) => samples,
      _ => &[],
    }
  }

  fn ulong_data(&self, channel: i32) -> &[u64] {
    match self.samples(channel) {
      Some(ChannelSamples::ULong(samples)) => samples,
      _ => &[],
    }
  }

  fn num_channels(&self) -> i32 {
    self.channels.len() as i32
  }

  fn sample_interval(&self, channel: i32) -> Duration {
    self.channels[channel as usize].sample_interval
  }

  fn name(&self, channel: i32) -> &str {
    &self.channels[channel as usize].name
  }

  fn is_short_data(&self) -> bool {
    self.is_short_data
  }

  fn is_int_data(&self) -> bool {
    self.is_int_data
  }

  fn is_ulong_data(&self) -> bool {
    self.is_ulong_data
  }

  fn is_transformed(&self) -> bool {
    self.is_transformed
  }

  fn buffer(&self) -> &[u8] {
    &self.buffer
  }

  fn encoding(&self) -> AnalogEncoding {
    self.encoding
  }

  fn analog_format(&self, channel: i32) -> AnalogFormat {
    match self.samples(channel) {
      Some(ChannelSamples::Short(_)) => AnalogFormat::Short,
      Some(ChannelSamples::Int(_)) => AnalogFormat::Int,
      Some(ChannelSamples::ULong(_)) => AnalogFormat::ULong,
      Some(ChannelSamples::Encoded) => AnalogFormat::Encoded,
      Some(ChannelSamples::Double(_)) | None => AnalogFormat::Double,
    }
  }

  fn encoded_count(&self) -> u64 {
    self.encoded_count
  }

  fn channels_changed(&self) -> bool {
    self.channels_changed
  }

  fn scale(&self, channel: i32) -> f64 {
    self.channels[channel as usize].scale
  }

  fn offset(&self, channel: i32) -> f64 {
    self.channels[channel as usize].offset
  }
}

/// A copy of an ImageData's planes and format.
pub struct ImageDataCopy {
  planes: Vec<Vec<u8>>,
  format: ImageFormat,
  width: u64,
  height: u64,
  frame_interval: Duration,
}

impl ImageDataCopy {
  pub fn new(original: &dyn ImageData) -> ImageDataCopy {
    ImageDataCopy {
      planes: (0..original.num_planes()).map(|i| original.plane(i as i32).to_vec()).collect(),
      format: original.format(),
      width: original.width(),
      height: original.height(),
      frame_interval: original.frame_interval(),
    }
  }
}

impl ImageData for ImageDataCopy {
  fn plane(&self, channel: i32) -> &[u8] {
    &self.planes[channel as usize]
  }

  fn num_planes(&self) -> u64 {
    self.planes.len() as u64
  }

  fn format(&self) -> ImageFormat {
    self.format
  }

  fn width(&self) -> u64 {
    self.width
  }

  fn height(&self) -> u64 {
    self.height
  }

  fn frame_interval(&self) -> Duration {
    self.frame_interval
  }
}

/// A copy of a NodeData's time and its analog and image data. Mocap and text
/// data aren't copied.
pub struct NodeDataCopy {
  time: Duration,
  analog: Option<AnalogDataCopy>,
  image: Option<ImageDataCopy>,
}

impl NodeDataCopy {
  pub fn new(original: &dyn NodeData) -> NodeDataCopy {
    NodeDataCopy {
      time: original.time(),
      analog: original.analog().map(AnalogDataCopy::new),
      image: original.image().map(ImageDataCopy::new),
    }
  }
}

impl NodeData for NodeDataCopy {
  fn time(&self) -> Duration {
    self.time
  }

  fn analog(&self) -> Option<&dyn AnalogData> {
    self.analog.as_ref().map(|a| a as &dyn AnalogData)
  }

  fn image(&self) -> Option<&dyn ImageData> {
    self.image.as_ref().map(|i| i as &dyn ImageData)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  struct Mixed;

  impl AnalogData for Mixed {
    fn data(&self, _: i32) -> &[f64] {
      &[1.0, 2.0]
    }
    fn short_data(&self, _: i32) -> &[i16] {
      &[3]
    }
    fn num_channels(&self) -> i32 {
      2
    }
    fn sample_interval(&self, channel: i32) -> Duration {
      Duration::from_millis(channel as u64 + 1)
    }
    fn name(&self, channel: i32) -> &str {
      ["a", "b"][channel as usize]
    }
    fn analog_format(&self, channel: i32) -> AnalogFormat {
      if channel == 0 { AnalogFormat::Double } else { AnalogFormat::Short }
    }
    fn channels_changed(&self) -> bool {
      true
    }
  }

  #[test]
  fn analog_copy_keeps_per_channel_formats() {
    let copy = AnalogDataCopy::new(&Mixed);
    assert_eq!(copy.num_channels(), 2);
    assert_eq!(copy.analog_format(0), AnalogFormat::Double);
    assert_eq!(copy.data(0), &[1.0, 2.0]);
    assert_eq!(copy.analog_format(1), AnalogFormat::Short);
    assert_eq!(copy.short_data(1), &[3]);
    assert_eq!(copy.name(1), "b");
    assert_eq!(copy.sample_interval(1), Duration::from_millis(2));
    assert!(copy.channels_changed());
  }

  #[test]
  fn copies_can_be_sent_between_threads() {
    fn assert_send<T: Send>() {}
    assert_send::<NodeDataCopy>();
  }
}
