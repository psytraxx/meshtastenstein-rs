use crate::{
    domain::{context::MeshCtx, handlers::util::send_ble_routing_result},
    ports::MeshStorage,
    proto::{Routing, routing},
};
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
            let entry = ctx.pending_packets.swap_remove(i);
            info!("[Mesh] ACK received for packet {:08x}", pkt.request_id);
            // Report the real mesh outcome to the phone now that it's known,
            // rather than the immediate "sent" confirmation this used to get
            // before the mesh had actually delivered anything.
            if let Some(notify_dest) = entry.ble_notify {
                send_ble_routing_result(ctx, notify_dest, entry.packet_id, routing::Error::None)
                    .await;
            }
        }

        // Learn route from ACK: the relay_node that forwarded this ACK
        // can reach the sender, so record it as next_hop for the sender
        ctx.router
            .learn_route(ctx.node_db, pkt.sender, pkt.relay_node);
    }
}
