//! Handler for PortNum::NeighborinfoApp

use super::RadioResult;
use log::debug;

pub fn handle(sender: u32, payload: &[u8]) -> RadioResult {
    debug!(
        "[PortHandler] NEIGHBORINFO from {:08x}: {} bytes",
        sender,
        payload.len()
    );
    RadioResult::default()
}
