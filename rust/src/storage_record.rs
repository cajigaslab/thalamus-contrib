//! The thalamus_grpc StorageRecord messages STORAGE3 writes, encoded with
//! prost from Thalamus's thalamus.proto (vendored in proto/, compiled by
//! build.rs), and the length prefixed framing of .tha files: each record is
//! preceded by its size as an 8-byte big-endian integer.

use prost::Message;

use crate::api::{
  AnalogData, AnalogEncoding, AnalogFormat, BlobData, EventMessage, ImageData, ImageFormat, LogMessage, MocapData,
  TextData,
};

/// The generated thalamus_grpc messages.
#[allow(clippy::all, dead_code)]
pub mod proto {
  include!(concat!(env!("OUT_DIR"), "/thalamus_grpc.rs"));
}

use proto::storage_record::Body;

/// Appends `record` to `output` with its 8-byte big-endian length prefix.
pub fn frame(output: &mut Vec<u8>, record: &[u8]) {
  output.extend_from_slice(&(record.len() as u64).to_be_bytes());
  output.extend_from_slice(record);
}

fn storage_record(body: Body, time_ns: u64, node: &str) -> Vec<u8> {
  proto::StorageRecord { body: Some(body), time: time_ns, node: node.to_string(), ..Default::default() }
    .encode_to_vec()
}

fn nanos(duration: std::time::Duration) -> u64 {
  duration.as_nanos() as u64
}

/// An AnalogResponse with every channel in its own format plus the encoded
/// buffer, as STORAGE2 writes it (append_native_channel and finish_native in
/// Thalamus's analog_proto.hpp).
pub fn analog(node: &str, time_ns: u64, analog: &dyn AnalogData) -> Vec<u8> {
  let mut body = proto::AnalogResponse::default();
  let mut any_array = false;
  let mut all_int = true;
  let mut all_ulong = true;
  let transformed = analog.is_transformed();
  for channel in 0..analog.num_channels() {
    let format = analog.analog_format(channel);
    let (begin, end, span_format) = match format {
      AnalogFormat::Double => {
        let begin = body.data.len();
        body.data.extend_from_slice(analog.data(channel));
        (begin, body.data.len(), proto::span::Format::Double)
      }
      AnalogFormat::Short => {
        let begin = body.int_data.len();
        body.int_data.extend(analog.short_data(channel).iter().map(|&value| i32::from(value)));
        (begin, body.int_data.len(), proto::span::Format::Short)
      }
      AnalogFormat::Int => {
        let begin = body.int_data.len();
        body.int_data.extend_from_slice(analog.int_data(channel));
        (begin, body.int_data.len(), proto::span::Format::Int)
      }
      AnalogFormat::ULong => {
        let begin = body.ulong_data.len();
        body.ulong_data.extend_from_slice(analog.ulong_data(channel));
        (begin, body.ulong_data.len(), proto::span::Format::ULong)
      }
      AnalogFormat::Encoded => (0, 0, proto::span::Format::Encoded),
    };
    if format != AnalogFormat::Encoded {
      any_array = true;
      all_int &= matches!(format, AnalogFormat::Short | AnalogFormat::Int);
      all_ulong &= format == AnalogFormat::ULong;
    }
    body.spans.push(proto::Span {
      begin: begin as u32,
      end: end as u32,
      name: analog.name(channel).to_string(),
      scale: if transformed { analog.scale(channel) } else { 0.0 },
      offset: if transformed { analog.offset(channel) } else { 0.0 },
      format: span_format as i32,
    });
    body.sample_intervals.push(nanos(analog.sample_interval(channel)));
  }
  body.is_int_data = any_array && all_int;
  body.is_ulong_data = any_array && all_ulong;
  body.is_transformed = transformed;
  body.time = time_ns;
  body.remote_time = nanos(analog.remote_time());
  body.buffer = analog.buffer().to_vec();
  body.encoding = match analog.encoding() {
    AnalogEncoding::None => proto::analog_response::Encoding::None,
    AnalogEncoding::AAC => proto::analog_response::Encoding::Aac,
    AnalogEncoding::Opus => proto::analog_response::Encoding::Opus,
    AnalogEncoding::FLAC => proto::analog_response::Encoding::Flac,
  } as i32;
  body.encoded_count = analog.encoded_count();
  storage_record(Body::Analog(body), time_ns, node)
}

/// thalamus_grpc::Image::Format, whose values differ from the plugin API's.
fn image_format(format: ImageFormat) -> proto::image::Format {
  use proto::image::Format;
  match format {
    ImageFormat::Gray => Format::Gray,
    ImageFormat::RGB => Format::Rgb,
    ImageFormat::YUYV422 => Format::Yuyv422,
    ImageFormat::YUV420P => Format::Yuv420p,
    ImageFormat::YUVJ420P => Format::Yuvj420p,
    ImageFormat::Gray16 => Format::Gray16,
    ImageFormat::RGB16 => Format::Rgb16,
    ImageFormat::MPEG1 => Format::Mpeg1,
    ImageFormat::MPEG4 => Format::Mpeg4,
    ImageFormat::NV12 => Format::Nv12,
    ImageFormat::BGR => Format::Bgr,
    ImageFormat::MJPEG => Format::Mjpeg,
    ImageFormat::H264 => Format::H264,
    ImageFormat::VP9 => Format::Vp9,
  }
}

pub fn image(node: &str, time_ns: u64, image: &dyn ImageData) -> Vec<u8> {
  let body = proto::Image {
    data: (0..image.num_planes()).map(|plane| image.plane(plane as i32).to_vec()).collect(),
    width: image.width() as u32,
    height: image.height() as u32,
    format: image_format(image.format()) as i32,
    frame_interval: nanos(image.frame_interval()),
    time: time_ns,
    remote_time: nanos(image.remote_time()),
    ..Default::default()
  };
  storage_record(Body::Image(body), time_ns, node)
}

/// An XsensResponse.
pub fn mocap(node: &str, time_ns: u64, mocap: &dyn MocapData) -> Vec<u8> {
  let body = proto::XsensResponse {
    segments: mocap
      .segments()
      .iter()
      .map(|segment| proto::XsensSegment {
        id: segment.segment_id,
        x: segment.position[0],
        y: segment.position[1],
        z: segment.position[2],
        q0: segment.rotation[0],
        q1: segment.rotation[1],
        q2: segment.rotation[2],
        q3: segment.rotation[3],
        frame: segment.frame,
        time: segment.time,
        actor: u32::from(segment.actor),
      })
      .collect(),
    pose_name: mocap.pose_name().to_string(),
    time: time_ns,
    remote_time: nanos(mocap.remote_time()),
  };
  storage_record(Body::Xsens(body), time_ns, node)
}

pub fn text(node: &str, time_ns: u64, text: &dyn TextData) -> Vec<u8> {
  let body = proto::Text {
    text: text.text().to_string(),
    time: time_ns,
    remote_time: nanos(text.remote_time()),
    ..Default::default()
  };
  storage_record(Body::Text(body), time_ns, node)
}

pub fn blob(node: &str, time_ns: u64, blob: &dyn BlobData) -> Vec<u8> {
  let body = proto::Blob { body: blob.body().to_vec(), mime: blob.mime().to_string(), stream: blob.stream() };
  storage_record(Body::Blob(body), time_ns, node)
}

/// A record of an event.
pub fn event(event: &EventMessage) -> Vec<u8> {
  let time_ns = nanos(event.time);
  storage_record(Body::Event(proto::Event { payload: event.payload.to_vec(), time: time_ns }), time_ns, "")
}

/// A record of a log message.
pub fn log(log: &LogMessage) -> Vec<u8> {
  let time_ns = nanos(log.time);
  let body = proto::Text {
    text: log.text.to_string(),
    time: time_ns,
    remote_time: nanos(log.remote_time),
    ..Default::default()
  };
  storage_record(Body::Text(body), time_ns, "")
}

/// The value of a metadata Pair.
#[derive(Debug, Clone, PartialEq)]
pub enum MetadataValue {
  Text(String),
  Decimal(f64),
  Integral(i64),
}

/// A Metadata record. Pairs without a value only have a key, as STORAGE2
/// writes rows whose value isn't text or a number.
pub fn metadata(node: &str, time_ns: u64, pairs: &[(String, Option<MetadataValue>)]) -> Vec<u8> {
  use proto::pair::Value;
  let body = proto::Metadata {
    keyvalues: pairs
      .iter()
      .map(|(key, value)| proto::Pair {
        key: key.clone(),
        value: value.as_ref().map(|value| match value {
          MetadataValue::Text(text) => Value::Text(text.clone()),
          MetadataValue::Decimal(decimal) => Value::Decimal(*decimal),
          MetadataValue::Integral(integral) => Value::Integral(*integral),
        }),
      })
      .collect(),
  };
  storage_record(Body::Metadata(body), time_ns, node)
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::Duration;

  fn decode(record: &[u8]) -> proto::StorageRecord {
    proto::StorageRecord::decode(record).unwrap()
  }

  struct TestAnalog;

  impl AnalogData for TestAnalog {
    fn data(&self, _channel: i32) -> &[f64] {
      &[1.0, -2.0]
    }
    fn short_data(&self, _channel: i32) -> &[i16] {
      &[-1, 2]
    }
    fn num_channels(&self) -> i32 {
      3
    }
    fn sample_interval(&self, _channel: i32) -> Duration {
      Duration::from_millis(1)
    }
    fn name(&self, channel: i32) -> &str {
      ["a", "b", "c"][channel as usize]
    }
    fn analog_format(&self, channel: i32) -> AnalogFormat {
      [AnalogFormat::Double, AnalogFormat::Short, AnalogFormat::Encoded][channel as usize]
    }
    fn buffer(&self) -> &[u8] {
      &[0xff, 0xf8]
    }
    fn encoding(&self) -> AnalogEncoding {
      AnalogEncoding::FLAC
    }
    fn encoded_count(&self) -> u64 {
      4608
    }
    fn remote_time(&self) -> Duration {
      Duration::from_nanos(3)
    }
  }

  #[test]
  fn records_are_framed_with_a_big_endian_length() {
    let mut output = Vec::new();
    frame(&mut output, &[7, 8, 9]);
    assert_eq!(output, vec![0, 0, 0, 0, 0, 0, 0, 3, 7, 8, 9]);
  }

  #[test]
  fn analog_records_keep_each_channel_in_its_format() {
    let record = decode(&analog("Mic", 5, &TestAnalog));
    assert_eq!((record.time, record.node.as_str()), (5, "Mic"));
    let Some(Body::Analog(body)) = record.body else { panic!("not analog: {:?}", record.body) };
    assert_eq!(body.data, vec![1.0, -2.0]);
    assert_eq!(body.int_data, vec![-1, 2]);
    let spans: Vec<_> = body.spans.iter().map(|s| (s.name.as_str(), s.begin, s.end, s.format())).collect();
    use proto::span::Format;
    assert_eq!(spans, [("a", 0, 2, Format::Double), ("b", 0, 2, Format::Short), ("c", 0, 0, Format::Encoded)]);
    assert_eq!(body.sample_intervals, vec![1_000_000; 3]);
    // Mixed formats: neither flag.
    assert!(!body.is_int_data && !body.is_ulong_data);
    assert_eq!((body.time, body.remote_time), (5, 3));
    assert_eq!(body.buffer, vec![0xff, 0xf8]);
    assert_eq!(body.encoding(), proto::analog_response::Encoding::Flac);
    assert_eq!(body.encoded_count, 4608);
  }

  #[test]
  fn events_and_log_messages_carry_their_times() {
    let ns = Duration::from_nanos;
    let record = decode(&event(&EventMessage { payload: b"hi", time: ns(10) }));
    assert_eq!(record.time, 10);
    assert_eq!(record.body, Some(Body::Event(proto::Event { payload: b"hi".to_vec(), time: 10 })));

    let record = decode(&log(&LogMessage { text: "log", time: ns(11), remote_time: ns(4) }));
    assert_eq!(record.time, 11);
    let Some(Body::Text(text)) = record.body else { panic!("not text") };
    assert_eq!((text.text.as_str(), text.time, text.remote_time), ("log", 11, 4));
  }

  struct RemoteText;

  impl TextData for RemoteText {
    fn text(&self) -> &str {
      "t"
    }
    fn remote_time(&self) -> Duration {
      Duration::from_nanos(3)
    }
  }

  #[test]
  fn text_records_carry_their_time_and_remote_time() {
    let record = decode(&text("n", 2, &RemoteText));
    assert_eq!((record.time, record.node.as_str()), (2, "n"));
    let Some(Body::Text(text)) = record.body else { panic!("not text") };
    assert_eq!((text.text.as_str(), text.time, text.remote_time), ("t", 2, 3));
  }

  #[test]
  fn metadata_values_are_kept_even_at_their_defaults() {
    use proto::pair::Value;
    let record = decode(&metadata("S3", 1, &[
      ("Rec".to_string(), Some(MetadataValue::Integral(0))),
      ("Text".to_string(), Some(MetadataValue::Text(String::new()))),
      ("None".to_string(), None),
    ]));
    let Some(Body::Metadata(body)) = record.body else { panic!("not metadata") };
    let pairs: Vec<_> = body.keyvalues.iter().map(|pair| (pair.key.as_str(), pair.value.clone())).collect();
    assert_eq!(pairs, [
      ("Rec", Some(Value::Integral(0))),
      ("Text", Some(Value::Text(String::new()))),
      ("None", None),
    ]);
  }

  #[test]
  fn plugin_image_formats_map_to_the_protobuf_values() {
    // The plugin API's MPEG1 is 8, the protobuf's 7.
    assert_eq!(image_format(ImageFormat::MPEG1) as i32, 7);
    assert_eq!(image_format(ImageFormat::NV12) as i32, 9);
  }
}
