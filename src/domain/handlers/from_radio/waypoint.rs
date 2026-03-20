use crate::domain::context::MeshCtx;
use crate::ports::MeshStorage;
use crate::proto::Waypoint;
use log::{info, warn};
use prost::Message;

pub async fn handle<S: MeshStorage>(_ctx: &mut MeshCtx<'_, S>, sender: u32, payload: &[u8]) {
    let waypoint = match Waypoint::decode(payload) {
        Ok(w) => w,
        Err(e) => {
            warn!(
                "[PortHandler] Waypoint decode failed from {:08x}: {:?}",
                sender, e
            );
            return;
        }
    };

    info!(
        "[PortHandler] Waypoint from {:08x}: id={} name=\"{}\" lat={:?} lon={:?}",
        sender, waypoint.id, waypoint.name, waypoint.latitude_i, waypoint.longitude_i,
    );
    // BLE forwarding is handled by the central dispatch in from_radio/mod.rs
}
