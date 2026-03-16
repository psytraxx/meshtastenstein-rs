//! Handler for PortNum::RoutingApp

use super::HandleResult;
use log::debug;

pub fn handle_routing_app(sender: u32, payload: &[u8], request_id: u32) -> HandleResult {
    debug!(
        "[PortHandler] ROUTING from {:08x}: {} bytes",
        sender,
        payload.len()
    );
    HandleResult {
        clear_ack_id: if request_id != 0 {
            Some(request_id)
        } else {
            None
        },
        ..HandleResult::default()
    }
}
