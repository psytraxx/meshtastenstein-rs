use crate::domain::context::MeshCtx;
use crate::domain::node_db::NodeDB;
use crate::ports::MeshStorage;
use embassy_time::{Duration, Timer};
use log::info;

pub async fn handle_begin_edit() {
    info!("[Admin] BeginEditSettings");
}

pub async fn handle_commit_edit() {
    info!("[Admin] CommitEditSettings");
}

pub async fn handle_reboot(secs: u32) {
    info!("[Admin] Rebooting in {} seconds", secs);
    Timer::after(Duration::from_secs(secs as u64)).await;
    esp_hal::system::software_reset();
}

pub async fn handle_factory_reset<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>) {
    info!("[Admin] Factory reset requested, rebooting in 2s");
    ctx.storage.erase_config();
    ctx.storage.clear_bond();
    Timer::after(Duration::from_secs(2)).await;
    esp_hal::system::software_reset();
}

pub async fn handle_nodedb_reset<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>) {
    info!("[Admin] NodeDB reset requested");
    *ctx.node_db = NodeDB::new(ctx.device.my_node_num);
}

pub async fn handle_shutdown(secs: u32) {
    info!("[Admin] Shutdown in {} seconds", secs);
}

pub async fn handle_remove_node<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, node_num: u32) {
    info!("[Admin] Removing node {:08x}", node_num);
    ctx.node_db.remove(node_num);
}
