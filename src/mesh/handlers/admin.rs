//! Handler for PortNum::AdminApp

use super::HandleResult;
use log::debug;

/// Called when an AdminApp packet arrives over LoRa or BLE.
/// Returns `admin_for_us = true` so mesh_task calls `handle_admin_from_ble` and
/// skips forwarding to LoRa.
pub fn handle_admin_app(sender: u32, payload: &[u8], addressed_to_us: bool) -> HandleResult {
    debug!(
        "[PortHandler] ADMIN from {:08x}: {} bytes (for_us={})",
        sender,
        payload.len(),
        addressed_to_us
    );
    HandleResult {
        forward_to_ble: !addressed_to_us,
        admin_for_us: addressed_to_us,
        ..HandleResult::default()
    }
}
