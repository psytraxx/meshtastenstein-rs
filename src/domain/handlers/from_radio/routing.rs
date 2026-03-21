use crate::domain::context::MeshCtx;
use crate::ports::MeshStorage;
use crate::proto::Routing;
use log::{info, warn};
use prost::Message;

pub async fn handle<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    sender: u32,
    relay_node: u8,
    payload: &[u8],
    request_id: u32,
) {
    let _routing = match Routing::decode(payload) {
        Ok(r) => r,
        Err(e) => {
            warn!(
                "[PortHandler] Could not decode Routing from {:08x}: {:?}",
                sender, e
            );
            return;
        }
    };

    info!(
        "[PortHandler] Routing message from {:08x}, request_id={:08x}",
        sender, request_id
    );

    // Clear pending packet if routing ACK received
    if request_id != 0 {
        let idx = ctx
            .pending_packets
            .iter()
            .position(|a| a.packet_id == request_id);
        if let Some(i) = idx {
            ctx.pending_packets.swap_remove(i);
            info!("[Mesh] ACK received for packet {:08x}", request_id);
        }

        // Learn route from ACK: the relay_node that forwarded this ACK
        // can reach the sender, so record it as next_hop for the sender
        ctx.router
            .learn_route(ctx.node_db, sender, relay_node, request_id);
    }
}
