//! Dispatch for packets arriving from the BLE app (phone → device).
//!
//! Most portnum types just get transmitted to LoRa unchanged. Only a small
//! number need pre-transmit action: position payloads are saved for
//! re-broadcast, admin packets addressed to us are handled locally.
//!
//! `dispatch()` is pure (no async, no Embassy). The returned `AppAction`
//! drives all side-effects in `mesh_task`.

pub mod position;

use crate::proto::PortNum;

// ── Context ───────────────────────────────────────────────────────────────────

/// Fields from a BLE-originated `MeshPacket` needed to classify it.
pub struct AppContext<'a> {
    pub portnum: u32,
    pub payload: &'a [u8],
    pub to: u32,
    pub my_node_num: u32,
}

// ── Result ────────────────────────────────────────────────────────────────────

/// What `mesh_task` must do with a BLE-originated packet.
pub enum AppAction {
    /// Drop silently — `UnknownApp` with empty payload.
    Drop,
    /// Save `payload` as our own position bytes for periodic re-broadcast,
    /// then transmit to LoRa.
    SavePositionAndTransmit,
    /// Decode as `AdminMessage` and handle locally; do not transmit to LoRa.
    HandleAdminLocally,
    /// Transmit to LoRa unchanged.
    Transmit,
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

pub fn dispatch(ctx: &AppContext<'_>) -> AppAction {
    let addressed_to_us = ctx.to == ctx.my_node_num;

    match PortNum::try_from(ctx.portnum as i32).ok() {
        Some(PortNum::UnknownApp) if ctx.payload.is_empty() => AppAction::Drop,

        Some(PortNum::PositionApp) => position::handle(ctx),

        Some(PortNum::AdminApp) if addressed_to_us => AppAction::HandleAdminLocally,

        _ => AppAction::Transmit,
    }
}
