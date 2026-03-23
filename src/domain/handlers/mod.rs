//! Packet handlers — one module per data-flow direction.
//!
//! - [`from_radio`] — packets arriving from the LoRa radio
//! - [`from_app`]   — packets arriving from the BLE app (phone → device)
//! - [`admin`]      — `AdminMessage` dispatch, shared by both directions
//! - [`periodic`]   — periodic broadcast tasks

pub mod admin;
pub mod from_app;
pub mod from_radio;
pub mod outgoing;
pub mod periodic;
pub mod util;

use crate::{
    domain::{context::MeshCtx, handlers::periodic::send_device_telemetry},
    inter_task::channels::MeshEvent,
    ports::MeshStorage,
};
use log::info;

pub async fn dispatch<S: MeshStorage>(event: MeshEvent, ctx: &mut MeshCtx<'_, S>) {
    match event {
        MeshEvent::LoraRx(frame, meta) => {
            from_radio::dispatch(ctx, *frame, meta).await;
        }
        MeshEvent::BleRx(msg) => {
            from_app::dispatch(ctx, *msg).await;
        }
        MeshEvent::BleConnected => {
            *ctx.ble_connected = true;
            info!("[Mesh] BLE connected");
        }
        MeshEvent::BleDisconnected => {
            *ctx.ble_connected = false;
            info!("[Mesh] BLE disconnected");
        }
        MeshEvent::BondSave(bytes) => {
            ctx.storage.save_bond(&bytes);
        }
        MeshEvent::BatteryUpdate(level, voltage_mv) => {
            send_device_telemetry(ctx, level, voltage_mv).await;
        }
        MeshEvent::ChannelUtilUpdate(c, a) => {
            ctx.channel_metrics.channel_util = c;
            ctx.channel_metrics.air_util_tx = a;
        }
        MeshEvent::Tick => {
            periodic::dispatch(ctx).await;
        }
    }
}
