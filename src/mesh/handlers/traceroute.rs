//! Handler for PortNum::TracerouteApp

use super::HandleResult;
use log::debug;

pub fn handle_traceroute_app(sender: u32, payload: &[u8]) -> HandleResult {
    debug!(
        "[PortHandler] TRACEROUTE from {:08x}: {} bytes",
        sender,
        payload.len()
    );
    HandleResult::default()
}
