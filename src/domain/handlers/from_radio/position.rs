use crate::domain::context::MeshCtx;
use crate::domain::handlers::util;
use crate::ports::MeshStorage;
use crate::proto::Position;
use log::{info, warn};
use prost::Message;

pub async fn handle<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, pkt: &super::InboundPacket<'_>) {
    let pos = match Position::decode(pkt.payload) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                "[PortHandler] Could not decode Position from {:08x}: {:?}",
                pkt.sender, e
            );
            return;
        }
    };

    info!(
        "[PortHandler] Position from {:08x}: lat={:?} lon={:?}",
        pkt.sender, pos.latitude_i, pos.longitude_i
    );

    ctx.node_db.update_position(pkt.sender, pos);
    util::notify_ble_node_update(ctx, pkt.sender).await;
}
