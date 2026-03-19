use crate::domain::context::MeshCtx;
use crate::ports::MeshStorage;
use log::info;

pub async fn handle<S: MeshStorage>(_ctx: &mut MeshCtx<'_, S>, sender: u32, _payload: &[u8]) {
    info!("[PortHandler] Telemetry from {:08x} (ignored)", sender);
}
