//! Handler for PortNum::TextMessageApp

use super::RadioResult;
use log::info;

pub fn handle(sender: u32, payload: &[u8]) -> RadioResult {
    info!(
        "[PortHandler] TEXT_MESSAGE from {:08x}: {} bytes",
        sender,
        payload.len()
    );
    RadioResult {
        buffer_if_offline: true,
        ..RadioResult::default()
    }
}
