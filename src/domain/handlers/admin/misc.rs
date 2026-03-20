use crate::domain::context::MeshCtx;
use crate::domain::handlers::admin::send_admin_response;
use crate::domain::node_db::NodeDB;
use crate::ports::MeshStorage;
use crate::proto::admin_message;
use embassy_time::{Duration, Timer};
use log::info;

pub async fn handle_begin_edit<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    requester: u32,
    req_pkt_id: u32,
) {
    info!("[Admin] BeginEditSettings");
    send_admin_response(
        ctx,
        requester,
        req_pkt_id,
        admin_message::PayloadVariant::BeginEditSettings(true),
    )
    .await;
}

pub async fn handle_commit_edit<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    requester: u32,
    req_pkt_id: u32,
) {
    info!("[Admin] CommitEditSettings");
    send_admin_response(
        ctx,
        requester,
        req_pkt_id,
        admin_message::PayloadVariant::CommitEditSettings(true),
    )
    .await;
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
    info!("[Admin] Shutdown in {} seconds — entering deep sleep", secs);
    Timer::after(Duration::from_secs(secs as u64)).await;
    // Deep sleep requires hardware peripherals not available in MeshCtx.
    // Software reset is the safe fallback: device reboots but won't reconnect
    // without a phone-initiated BLE connection.
    esp_hal::system::software_reset();
}

pub async fn handle_remove_node<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, node_num: u32) {
    info!("[Admin] Removing node {:08x}", node_num);
    ctx.node_db.remove(node_num);
}
