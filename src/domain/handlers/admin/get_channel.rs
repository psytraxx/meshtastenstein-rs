use crate::domain::context::MeshCtx;
use crate::domain::handlers::admin::send_admin_response;
use crate::ports::MeshStorage;
use crate::proto::{Channel, ChannelSettings, admin_message};
use log::debug;

pub async fn handle<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    requester: u32,
    req_pkt_id: u32,
    idx_plus_1: u32,
) {
    debug!("[Admin] Handling GetChannelRequest: {}", idx_plus_1);

    let idx = if idx_plus_1 == 0 { 0 } else { idx_plus_1 - 1 } as u8;

    let ch_msg = if let Some(ch) = ctx.device.channels.get(idx) {
        Channel {
            index: idx as i32,
            settings: Some(ChannelSettings {
                psk: ch.psk.to_vec(),
                name: ch.name.as_str().into(),
                ..Default::default()
            }),
            role: ch.role as i32,
        }
    } else {
        Channel {
            index: idx as i32,
            settings: None,
            role: 0, // DISABLED
        }
    };

    send_admin_response(
        ctx,
        requester,
        req_pkt_id,
        admin_message::PayloadVariant::GetChannelResponse(ch_msg),
    )
    .await;
}
