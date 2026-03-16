//! Central dispatch for mesh packets by PortNum.
//!
//! Two entry points:
//! - `dispatch()` — incoming LoRa packets; operates on `PortNumContext` + `NodeDB`
//! - `classify_outgoing()` — outgoing BLE→LoRa packets; pure classification, no NodeDB
//!
//! Each portnum variant lives in its own submodule. All handlers are pure
//! (no async, no Embassy references); side-effects are performed by the caller.

pub mod admin;
pub mod neighbor_info;
pub mod node_info;
pub mod position;
pub mod remote_hardware;
pub mod routing;
pub mod telemetry;
pub mod text_message;
pub mod traceroute;
pub mod waypoint;

use crate::mesh::node_db::NodeDB;
use crate::proto::PortNum;
use log::warn;

// ── Context ───────────────────────────────────────────────────────────────────

/// All fields extracted from a decoded `Data` protobuf, passed to `dispatch()`.
pub struct PortNumContext<'a> {
    pub portnum: i32,
    pub payload: &'a [u8],
    pub sender: u32,
    /// `Data.want_response` — handler sets `reply_with_nodeinfo` accordingly
    pub want_response: bool,
    /// `Data.request_id` — used by routing ACK handler to identify the ACK'd packet
    pub request_id: u32,
    /// Whether this packet's destination matches our node num (needed by admin handler)
    pub addressed_to_us: bool,
}

// ── Result ────────────────────────────────────────────────────────────────────

/// Flags returned to `mesh_task` — each flag triggers an async side-effect.
///
/// All fields default to `false` / `None`; handlers only set what they need.
pub struct HandleResult {
    /// Forward the raw MeshPacket to BLE.
    /// `false` only when an admin packet is addressed to us (handled locally).
    pub forward_to_ble: bool,
    /// Send our `NodeInfo` back to `sender` (`NodeinfoApp` with `want_response`).
    pub reply_with_nodeinfo: bool,
    /// Send an additional `NodeInfo` `FromRadio` to BLE after a NodeDB update.
    pub notify_ble_of_node_update: bool,
    /// Buffer this frame in NVS ring when BLE is offline (TextMessage only).
    pub buffer_if_offline: bool,
    /// Routing ACK: clear this `request_id` from `pending_acks`. `None` = no-op.
    pub clear_ack_id: Option<u32>,
    /// Admin message addressed to us — `mesh_task` must call `handle_admin_from_ble`
    /// and skip forwarding to LoRa.
    pub admin_for_us: bool,
}

impl Default for HandleResult {
    fn default() -> Self {
        Self {
            forward_to_ble: true,
            reply_with_nodeinfo: false,
            notify_ble_of_node_update: false,
            buffer_if_offline: false,
            clear_ack_id: None,
            admin_for_us: false,
        }
    }
}

// ── Central dispatch ──────────────────────────────────────────────────────────

/// Dispatch a decoded Data message to the appropriate portnum handler.
///
/// Pure: no async, no Embassy types, no hardware access.
/// The returned `HandleResult` drives all side-effects in the calling async task.
pub fn dispatch(ctx: &PortNumContext<'_>, node_db: &mut NodeDB) -> HandleResult {
    match PortNum::try_from(ctx.portnum).ok() {
        Some(PortNum::RemoteHardwareApp) => {
            remote_hardware::handle_remote_hardware_app(ctx.sender, ctx.payload)
        }

        Some(PortNum::TextMessageApp) => {
            text_message::handle_text_message_app(ctx.sender, ctx.payload)
        }

        Some(PortNum::TextMessageCompressedApp) => {
            // N1: phone app decompresses; we forward as-is (no Unishox2 on device)
            text_message::handle_text_message_app(ctx.sender, ctx.payload)
        }

        Some(PortNum::PositionApp) => {
            position::handle_position_app(ctx.sender, ctx.payload, node_db)
        }

        Some(PortNum::NodeinfoApp) => {
            node_info::handle_nodeinfo_app(ctx.sender, ctx.payload, ctx.want_response, node_db)
        }

        Some(PortNum::RoutingApp) => {
            routing::handle_routing_app(ctx.sender, ctx.payload, ctx.request_id)
        }

        Some(PortNum::AdminApp) => HandleResult {
            // Admin packets addressed to us are handled locally by mesh_task;
            // admin packets for others are forwarded to BLE as normal.
            forward_to_ble: !ctx.addressed_to_us,
            admin_for_us: ctx.addressed_to_us,
            ..HandleResult::default()
        },

        Some(PortNum::WaypointApp) => waypoint::handle_waypoint_app(ctx.sender, ctx.payload),

        Some(PortNum::TelemetryApp) => telemetry::handle_telemetry_app(ctx.sender, ctx.payload),

        Some(PortNum::TracerouteApp) => traceroute::handle_traceroute_app(ctx.sender, ctx.payload),

        Some(PortNum::NeighborinfoApp) => {
            neighbor_info::handle_neighborinfo_app(ctx.sender, ctx.payload)
        }

        _ => {
            warn!(
                "[PortHandler] Unknown portnum {} from {:08x}",
                ctx.portnum, ctx.sender
            );
            HandleResult::default()
        }
    }
}

// ── Outgoing BLE→LoRa classification ─────────────────────────────────────────

/// Decision for an outgoing packet originating from BLE.
pub enum OutgoingBleAction {
    /// Drop silently (UnknownApp with empty payload).
    Drop,
    /// Save payload as our own position for periodic re-broadcast, then transmit.
    SavePosition,
    /// Admin packet addressed to us — handle locally, do not transmit to LoRa.
    HandleAdminLocally,
    /// Transmit to LoRa unchanged.
    Transmit,
}

/// Classify a BLE-originated packet before transmission.
///
/// `portnum` is the raw `u32` from `data.portnum as u32`.
/// `payload_empty` should be `inner_payload.is_empty()`.
/// `addressed_to_us` should be `to == my_node_num`.
pub fn classify_outgoing(
    portnum: u32,
    payload_empty: bool,
    addressed_to_us: bool,
) -> OutgoingBleAction {
    match PortNum::try_from(portnum as i32).ok() {
        Some(PortNum::UnknownApp) if payload_empty => OutgoingBleAction::Drop,
        Some(PortNum::PositionApp) => OutgoingBleAction::SavePosition,
        Some(PortNum::AdminApp) if addressed_to_us => OutgoingBleAction::HandleAdminLocally,
        _ => OutgoingBleAction::Transmit,
    }
}
