//! Handler for PortNum::TextMessageApp

use super::HandleResult;
use log::info;

pub fn handle_text_message_app(sender: u32, payload: &[u8]) -> HandleResult {
    info!(
        "[PortHandler] TEXT_MESSAGE from {:08x}: {} bytes",
        sender,
        payload.len()
    );
    HandleResult {
        buffer_if_offline: true,
        ..HandleResult::default()
    }
}
