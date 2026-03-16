//! Handler for PortNum::NeighborinfoApp

use super::HandleResult;
use log::debug;

pub fn handle_neighborinfo_app(sender: u32, payload: &[u8]) -> HandleResult {
    debug!(
        "[PortHandler] NEIGHBORINFO from {:08x}: {} bytes",
        sender,
        payload.len()
    );
    HandleResult::default()
}
