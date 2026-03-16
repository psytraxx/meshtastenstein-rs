//! Handler for PortNum::RoutingApp

use super::RadioResult;
use log::debug;

pub fn handle(sender: u32, payload: &[u8], request_id: u32) -> RadioResult {
    debug!(
        "[PortHandler] ROUTING from {:08x}: {} bytes",
        sender,
        payload.len()
    );
    RadioResult {
        clear_ack_id: if request_id != 0 {
            Some(request_id)
        } else {
            None
        },
        ..RadioResult::default()
    }
}
