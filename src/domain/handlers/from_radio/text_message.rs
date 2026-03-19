use crate::domain::context::MeshCtx;
use crate::domain::packet::RadioFrame;
use crate::ports::MeshStorage;
use log::info;

pub async fn handle<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    frame: &RadioFrame,
    sender: u32,
    payload: &[u8],
) {
    if let Ok(text) = core::str::from_utf8(payload) {
        info!(
            "[PortHandler] Text message from {:08x}: \"{}\"",
            sender, text
        );
    } else {
        info!(
            "[PortHandler] Binary message (len={}) from {:08x}",
            payload.len(),
            sender
        );
    }

    // Buffer text messages when BLE is disconnected
    if !*ctx.ble_connected {
        let _ = ctx.storage.add(frame);
        info!("[Mesh] Buffered TEXT_MESSAGE from {:08x}", sender);
    }
}
