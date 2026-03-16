//! Builds the TelemetryApp payload (a serialised `Telemetry` proto).

use crate::proto::{DeviceMetrics, Telemetry, telemetry};
use prost::Message;

pub fn build_payload(battery_level: u8, voltage_v: f32) -> alloc::vec::Vec<u8> {
    Telemetry {
        time: 0,
        variant: Some(telemetry::Variant::DeviceMetrics(DeviceMetrics {
            battery_level: Some(battery_level as u32),
            voltage: Some(voltage_v),
            ..Default::default()
        })),
    }
    .encode_to_vec()
}
