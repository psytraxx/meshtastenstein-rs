use crate::{domain::context::MeshCtx, ports::MeshStorage, proto::NeighborInfo};
use log::{info, warn};
use prost::Message;

pub async fn handle<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, pkt: &super::InboundPacket<'_>) {
    let neighbor_info = match NeighborInfo::decode(pkt.payload) {
        Ok(n) => n,
        Err(e) => {
            warn!(
                "[PortHandler] NeighborInfo decode failed from {:08x}: {:?}",
                pkt.sender, e
            );
            return;
        }
    };

    info!(
        "[PortHandler] NeighborInfo from {:08x}: {} neighbor(s)",
        pkt.sender,
        neighbor_info.neighbors.len()
    );
    for nb in &neighbor_info.neighbors {
        info!(
            "[PortHandler]   neighbor {:08x} snr={:.1}",
            nb.node_id, nb.snr
        );
        // Touch NodeDB so we know this neighbor exists
        // `None`: this SNR is the *neighbour's* measurement of that node, not
        // ours. Recording it as our own would show the phone a link quality we
        // never observed, so the node is touched for liveness only.
        ctx.node_db.touch(nb.node_id, 0, None, 0);
    }
    // BLE forwarding is handled by the central dispatch in from_radio/mod.rs
}
