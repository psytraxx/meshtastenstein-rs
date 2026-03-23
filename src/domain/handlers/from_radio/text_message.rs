use crate::domain::context::MeshCtx;
use crate::ports::MeshStorage;
use log::info;

pub async fn handle<S: MeshStorage>(_ctx: &mut MeshCtx<'_, S>, pkt: &super::InboundPacket<'_>) {
    if let Ok(text) = core::str::from_utf8(pkt.payload) {
        info!(
            "[PortHandler] Text message from {:08x}: \"{}\"",
            pkt.sender, text
        );
    } else {
        info!(
            "[PortHandler] Binary message (len={}) from {:08x}",
            pkt.payload.len(),
            pkt.sender
        );
    }
    // Store-and-forward buffering is handled in from_radio/mod.rs (needs raw RadioFrame)
}
