use crate::{
    domain::{
        channels::{ChannelConfig, ChannelRole},
        context::MeshCtx,
    },
    ports::MeshStorage,
    proto::Channel,
};
use log::info;

pub async fn handle<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, ch: Channel) {
    info!("[Admin] Setting channel {}", ch.index);

    if let Some(settings) = ch.settings {
        let mut new_ch = ChannelConfig {
            index: ch.index as u8,
            name: heapless::String::new(),
            psk: heapless::Vec::new(),
            role: ChannelRole::try_from(ch.role).unwrap_or(ChannelRole::Secondary),
        };
        new_ch.name.push_str(&settings.name).ok();
        new_ch.psk.extend_from_slice(&settings.psk).ok();

        ctx.device.channels.set(ch.index as u8, new_ch);
        ctx.storage.save_state(ctx.device);
    }
}
