//! Handler for PortNum::TelemetryApp

use super::HandleResult;
use log::debug;

pub fn handle_telemetry_app(sender: u32, payload: &[u8]) -> HandleResult {
    debug!(
        "[PortHandler] TELEMETRY from {:08x}: {} bytes",
        sender,
        payload.len()
    );
    HandleResult::default()
}
