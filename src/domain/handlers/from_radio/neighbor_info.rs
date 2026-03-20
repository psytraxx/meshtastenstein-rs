use crate::domain::context::MeshCtx;
use crate::ports::MeshStorage;
use crate::proto::NeighborInfo;
use log::{info, warn};
use prost::Message;

pub async fn handle<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, sender: u32, payload: &[u8]) {
    let neighbor_info = match NeighborInfo::decode(payload) {
        Ok(n) => n,
        Err(e) => {
            warn!(
                "[PortHandler] NeighborInfo decode failed from {:08x}: {:?}",
                sender, e
            );
            return;
        }
    };

    info!(
        "[PortHandler] NeighborInfo from {:08x}: {} neighbor(s)",
        sender,
        neighbor_info.neighbors.len()
    );
    for nb in &neighbor_info.neighbors {
        info!(
            "[PortHandler]   neighbor {:08x} snr={:.1}",
            nb.node_id, nb.snr
        );
        // Touch NodeDB so we know this neighbor exists
        ctx.node_db.touch(nb.node_id, 0, nb.snr as i8, 0);
    }
    // BLE forwarding is handled by the central dispatch in from_radio/mod.rs
}
