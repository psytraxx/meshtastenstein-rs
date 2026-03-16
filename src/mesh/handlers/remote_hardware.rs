//! Handler for PortNum::RemoteHardwareApp

use super::HandleResult;
use log::debug;

pub fn handle_remote_hardware_app(sender: u32, payload: &[u8]) -> HandleResult {
    debug!(
        "[PortHandler] REMOTE_HARDWARE from {:08x}: {} bytes (forwarded to BLE)",
        sender,
        payload.len()
    );
    HandleResult::default()
}
