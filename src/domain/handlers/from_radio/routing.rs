use crate::domain::context::MeshCtx;
use crate::ports::MeshStorage;
use crate::proto::Routing;
use log::{info, warn};
use prost::Message;

pub async fn handle<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    sender: u32,
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

    // M1: Clear pending ACK if routing ACK received
    if request_id != 0 {
        let idx = ctx
            .pending_acks
            .iter()
            .position(|a| a.packet_id == request_id);
        if let Some(i) = idx {
            ctx.pending_acks.swap_remove(i);
            info!("[Mesh] ACK received for packet {:08x}", request_id);
        }
    }
}
