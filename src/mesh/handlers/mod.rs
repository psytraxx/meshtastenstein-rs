//! Central dispatch for incoming decoded mesh packets by PortNum.
//!
//! Each portnum variant lives in its own submodule with a `handle_*` function
//! that is pure (no async, no Embassy references). `dispatch()` calls the right
//! handler and returns `HandleResult` flags; the caller (`mesh_task`) performs
//! the resulting async side-effects.

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

        Some(PortNum::AdminApp) => {
            admin::handle_admin_app(ctx.sender, ctx.payload, ctx.addressed_to_us)
        }

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
