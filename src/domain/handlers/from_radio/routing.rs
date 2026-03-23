use crate::domain::context::MeshCtx;
use crate::ports::MeshStorage;
use crate::proto::Routing;
use log::{info, warn};
use prost::Message;

pub async fn handle<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, pkt: &super::InboundPacket<'_>) {
    let _routing = match Routing::decode(pkt.payload) {
        Ok(r) => r,
        Err(e) => {
            warn!(
                "[PortHandler] Could not decode Routing from {:08x}: {:?}",
                pkt.sender, e
            );
            return;
        }
    };

    info!(
        "[PortHandler] Routing message from {:08x}, request_id={:08x}",
        pkt.sender, pkt.request_id
    );

    // Clear pending packet if routing ACK received
    if pkt.request_id != 0 {
        let idx = ctx
            .pending_packets
            .iter()
            .position(|a| a.packet_id == pkt.request_id);
        if let Some(i) = idx {
            ctx.pending_packets.swap_remove(i);
            info!("[Mesh] ACK received for packet {:08x}", pkt.request_id);
        }

        // Learn route from ACK: the relay_node that forwarded this ACK
        // can reach the sender, so record it as next_hop for the sender
        ctx.router
            .learn_route(ctx.node_db, pkt.sender, pkt.relay_node);
    }
}
