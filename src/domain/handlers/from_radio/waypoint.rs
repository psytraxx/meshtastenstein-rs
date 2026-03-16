//! Handler for PortNum::WaypointApp

use super::RadioResult;
use log::debug;

pub fn handle(sender: u32, payload: &[u8]) -> RadioResult {
    // N2: forward to BLE; phone app renders waypoints
    debug!(
        "[PortHandler] WAYPOINT from {:08x}: {} bytes",
        sender,
        payload.len()
    );
    RadioResult::default()
}
