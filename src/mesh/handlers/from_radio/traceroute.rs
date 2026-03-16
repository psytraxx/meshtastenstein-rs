//! Handler for PortNum::TracerouteApp

use super::RadioResult;
use log::debug;

pub fn handle(sender: u32, payload: &[u8]) -> RadioResult {
    debug!(
        "[PortHandler] TRACEROUTE from {:08x}: {} bytes",
        sender,
        payload.len()
    );
    RadioResult::default()
}
