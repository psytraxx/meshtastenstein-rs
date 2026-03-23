use crate::constants::*;
use crate::domain::context::MeshCtx;
use crate::domain::handlers::util::{notify_ble_node_update, send_nodeinfo};
use crate::ports::MeshStorage;
use crate::proto::User;
use embassy_time::{Duration, Instant};
use log::{debug, info, warn};
use prost::Message;

pub async fn handle<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, pkt: &super::InboundPacket<'_>) {
    let user = match User::decode(pkt.payload) {
        Ok(u) => u,
        Err(e) => {
            warn!(
                "[PortHandler] Could not decode User from {:08x}: {:?}",
                pkt.sender, e
            );
            return;
        }
    };

    info!(
        "[PortHandler] NodeInfo from {:08x}: {} ({})",
        pkt.sender, user.long_name, user.short_name
    );

    ctx.node_db.update_user(pkt.sender, user);
    notify_ble_node_update(ctx, pkt.sender).await;

    // Respond to NodeInfo requests (throttled)
    if pkt.want_response {
        let throttled = ctx
            .last_nodeinfo_tx
            .map(|t| t.elapsed() < Duration::from_millis(NODEINFO_MIN_INTERVAL_MS))
            .unwrap_or(false);
        if throttled {
            debug!("[Mesh] NodeInfo request from {:08x} throttled", pkt.sender);
        } else {
            info!(
                "[Mesh] NodeInfo request from {:08x}, sending response",
                pkt.sender
            );
            send_nodeinfo(ctx, pkt.sender, false).await;
            *ctx.last_nodeinfo_tx = Some(Instant::now());
        }
    }
}
