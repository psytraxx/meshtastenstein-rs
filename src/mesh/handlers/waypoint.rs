//! Handler for PortNum::WaypointApp

use super::HandleResult;
use log::debug;

pub fn handle_waypoint_app(sender: u32, payload: &[u8]) -> HandleResult {
    // N2: forward to BLE; phone app renders waypoints
    debug!(
        "[PortHandler] WAYPOINT from {:08x}: {} bytes",
        sender,
        payload.len()
    );
    HandleResult::default()
}
