// Serial port settings shared by the nodes that read serial ports.
use crate::api::{State, StateKey, StateValue};
use serialport::{ClearBuffer, DataBits, FlowControl, Parity, SerialPort, StopBits};
use std::time::Duration;

// How often a serial read checks whether the node stopped. Senders write each
// record in one go, so an idle line means no record is in progress.
pub const SERIAL_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, PartialEq, Clone)]
pub struct SerialSettings {
  pub baud_rate: u32,
  pub data_bits: DataBits,
  pub parity: Parity,
  pub stop_bits: StopBits,
  pub flow_control: FlowControl,
  pub dtr: bool,
}

impl Default for SerialSettings {
  // 8N1 at arduino/thalamus_bridge's rate.
  fn default() -> Self {
    SerialSettings {
      baud_rate: 115200,
      data_bits: DataBits::Eight,
      parity: Parity::None,
      stop_bits: StopBits::One,
      flow_control: FlowControl::None,
      dtr: true,
    }
  }
}

fn parse_baud_rate(value: f64) -> Result<u32, String> {
  if value >= 1.0 && value <= f64::from(u32::MAX) && value.fract() == 0.0 {
    Ok(value as u32)
  } else {
    Err(format!("invalid Baud Rate {value}"))
  }
}

fn parse_data_bits(value: &str) -> Result<DataBits, String> {
  match value {
    "5" => Ok(DataBits::Five),
    "6" => Ok(DataBits::Six),
    "7" => Ok(DataBits::Seven),
    "8" => Ok(DataBits::Eight),
    _ => Err(format!("invalid Data Bits {value}")),
  }
}

fn parse_parity(value: &str) -> Result<Parity, String> {
  match value {
    "None" => Ok(Parity::None),
    "Odd" => Ok(Parity::Odd),
    "Even" => Ok(Parity::Even),
    _ => Err(format!("invalid Parity {value}")),
  }
}

fn parse_stop_bits(value: &str) -> Result<StopBits, String> {
  match value {
    "1" => Ok(StopBits::One),
    "2" => Ok(StopBits::Two),
    _ => Err(format!("invalid Stop Bits {value}")),
  }
}

fn parse_flow_control(value: &str) -> Result<FlowControl, String> {
  match value {
    "None" => Ok(FlowControl::None),
    "Software" => Ok(FlowControl::Software),
    "Hardware" => Ok(FlowControl::Hardware),
    _ => Err(format!("invalid Flow Control {value}")),
  }
}

// Reads the state's settings, keeping the default of any that aren't set.
pub fn serial_settings(state: &State) -> Result<SerialSettings, String> {
  let get = |key: &str| state.get(StateKey::String(key.to_string()));
  let mut settings = SerialSettings::default();
  if let Some(value) = get("Baud Rate").and_then(|value| f64::try_from(value).ok()) {
    settings.baud_rate = parse_baud_rate(value)?;
  }
  if let Some(StateValue::String(value)) = get("Data Bits") {
    settings.data_bits = parse_data_bits(&value)?;
  }
  if let Some(StateValue::String(value)) = get("Parity") {
    settings.parity = parse_parity(&value)?;
  }
  if let Some(StateValue::String(value)) = get("Stop Bits") {
    settings.stop_bits = parse_stop_bits(&value)?;
  }
  if let Some(StateValue::String(value)) = get("Flow Control") {
    settings.flow_control = parse_flow_control(&value)?;
  }
  if let Some(StateValue::Bool(value)) = get("DTR") {
    settings.dtr = value;
  }
  Ok(settings)
}

// DTR stays low until stale input is cleared: boards with native USB start
// sending, at the start of a record, when it goes high.
pub fn open_serial(path: &str, settings: &SerialSettings) -> Result<Box<dyn SerialPort>, serialport::Error> {
  let mut port = serialport::new(path, settings.baud_rate)
    .data_bits(settings.data_bits)
    .parity(settings.parity)
    .stop_bits(settings.stop_bits)
    .flow_control(settings.flow_control)
    .timeout(SERIAL_TIMEOUT)
    .dtr_on_open(false)
    .open()?;
  port.clear(ClearBuffer::Input)?;
  port.write_data_terminal_ready(settings.dtr)?;
  Ok(port)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_serial_settings() {
    assert_eq!(parse_baud_rate(9600.0), Ok(9600));
    assert!(parse_baud_rate(0.0).is_err() && parse_baud_rate(1.5).is_err());
    assert_eq!(parse_data_bits("7"), Ok(DataBits::Seven));
    assert!(parse_data_bits("9").is_err());
    assert_eq!(parse_parity("Even"), Ok(Parity::Even));
    assert!(parse_parity("").is_err());
    assert_eq!(parse_stop_bits("2"), Ok(StopBits::Two));
    assert!(parse_stop_bits("1.5").is_err());
    assert_eq!(parse_flow_control("Hardware"), Ok(FlowControl::Hardware));
    assert!(parse_flow_control("x").is_err());
  }
}
