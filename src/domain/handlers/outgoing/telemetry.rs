//! Builds the TelemetryApp payload (a serialised `Telemetry` proto).

use crate::proto::{DeviceMetrics, Telemetry, telemetry};
use prost::Message;

pub fn build_payload(
    battery_level: u8,
    voltage_v: f32,
    channel_utilization: f32,
    air_util_tx: f32,
    uptime_seconds: u32,
) -> alloc::vec::Vec<u8> {
    Telemetry {
        time: 0,
        variant: Some(telemetry::Variant::DeviceMetrics(DeviceMetrics {
            battery_level: Some(battery_level as u32),
            voltage: Some(voltage_v),
            channel_utilization: Some(channel_utilization),
            air_util_tx: Some(air_util_tx),
            uptime_seconds: Some(uptime_seconds),
        })),
    }
    .encode_to_vec()
}
