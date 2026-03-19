use crate::domain::context::MeshCtx;
use crate::ports::MeshStorage;
use log::info;

pub async fn handle<S: MeshStorage>(_ctx: &mut MeshCtx<'_, S>, _payload: &[u8]) {
    info!("[PortHandler] Position from BLE (ignored)");
}
