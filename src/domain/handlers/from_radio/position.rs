use crate::domain::context::MeshCtx;
use crate::ports::MeshStorage;
use crate::proto::Position;
use log::{info, warn};
use prost::Message;

pub async fn handle<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, sender: u32, payload: &[u8]) {
    let pos = match Position::decode(payload) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                "[PortHandler] Could not decode Position from {:08x}: {:?}",
                sender, e
            );
            return;
        }
    };

    info!(
        "[PortHandler] Position from {:08x}: lat={:?} lon={:?}",
        sender, pos.latitude_i, pos.longitude_i
    );

    ctx.node_db.update_position(sender, pos);
    notify_ble_node_update(ctx, sender).await;
}

async fn notify_ble_node_update<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, sender: u32) {
    crate::domain::handlers::util::notify_ble_node_update(ctx, sender).await;
}
