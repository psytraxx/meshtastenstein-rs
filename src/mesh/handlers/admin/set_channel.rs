//! Handler for AdminMessage::SetChannel

use super::{AdminContext, AdminResult};
use crate::mesh::channels::{ChannelConfig, ChannelRole};
use crate::proto::Channel;
use log::info;

pub fn handle(ctx: &mut AdminContext<'_>, ch: Channel) -> AdminResult {
    let idx = ch.index as u8;
    if let Some(settings) = ch.settings {
        let mut psk = heapless::Vec::new();
        psk.extend_from_slice(&settings.psk).ok();
        let mut name: heapless::String<12> = heapless::String::new();
        let _ = name.push_str(&settings.name[..settings.name.len().min(11)]);
        let role = match ch.role {
            1 => ChannelRole::Primary,
            2 => ChannelRole::Secondary,
            _ => ChannelRole::Disabled,
        };
        let new_ch = ChannelConfig {
            index: idx,
            name,
            psk,
            role,
        };
        info!(
            "[Admin] Channel {} updated: name='{}' role={:?} hash=0x{:02x} psk_len={}",
            idx,
            new_ch.name.as_str(),
            new_ch.role,
            new_ch.hash(ctx.device.modem_preset.display_name()),
            new_ch.effective_psk().len()
        );
        ctx.device.channels.set(idx, new_ch);
    }
    AdminResult {
        needs_persist: true,
        ..AdminResult::default()
    }
}
