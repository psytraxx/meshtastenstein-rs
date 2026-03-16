//! Handler for AdminMessage::GetChannelRequest

use super::{AdminContext, AdminResult};
use crate::proto::{Channel, ChannelSettings, admin_message};
use log::info;

pub fn handle(ctx: &AdminContext<'_>, idx_plus_1: u32) -> AdminResult {
    let idx = idx_plus_1.saturating_sub(1) as u8;
    info!("[Admin] GetChannelRequest idx={}", idx);
    let channel = if let Some(ch) = ctx.device.channels.get(idx) {
        Channel {
            index: idx as i32,
            settings: Some(ChannelSettings {
                psk: ch.effective_psk().to_vec(),
                name: ch.name.as_str().into(),
                ..Default::default()
            }),
            role: ch.role as i32,
        }
    } else {
        Channel {
            index: idx as i32,
            settings: None,
            role: 0, // Disabled
        }
    };
    AdminResult {
        response: Some(admin_message::PayloadVariant::GetChannelResponse(channel)),
        ..AdminResult::default()
    }
}
