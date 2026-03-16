//! Handler for PortNum::TelemetryApp

use super::RadioResult;
use log::debug;

pub fn handle(sender: u32, payload: &[u8]) -> RadioResult {
    debug!(
        "[PortHandler] TELEMETRY from {:08x}: {} bytes",
        sender,
        payload.len()
    );
    RadioResult::default()
}
