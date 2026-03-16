//! Handler for PortNum::RemoteHardwareApp

use super::RadioResult;
use log::debug;

pub fn handle(sender: u32, payload: &[u8]) -> RadioResult {
    debug!(
        "[PortHandler] REMOTE_HARDWARE from {:08x}: {} bytes (forwarded to BLE)",
        sender,
        payload.len()
    );
    RadioResult::default()
}
