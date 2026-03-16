//! Dispatch for packets arriving from the LoRa radio.
//!
//! `dispatch()` is pure (no async, no Embassy). It calls the right per-portnum
//! handler and returns `RadioResult` flags; `mesh_task` performs the async
//! side-effects.

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

/// Fields extracted from a decoded `Data` protobuf for a LoRa-received packet.
pub struct RadioContext<'a> {
    pub portnum: i32,
    pub payload: &'a [u8],
    pub sender: u32,
    /// `Data.want_response` — handler sets `reply_with_nodeinfo` accordingly
    pub want_response: bool,
    /// `Data.request_id` — used by routing ACK handler to clear pending ACKs
    pub request_id: u32,
    /// Whether this packet's destination matches our node num
    pub addressed_to_us: bool,
}

// ── Result ────────────────────────────────────────────────────────────────────

/// Flags returned to `mesh_task` — each flag triggers an async side-effect.
///
/// All fields default to `false` / `None`; handlers only set what they need.
pub struct RadioResult {
    /// Forward the raw MeshPacket to BLE.
    /// `false` only for admin packets addressed to us (handled locally).
    pub forward_to_ble: bool,
    /// Send our `NodeInfo` back to `sender` (`NodeinfoApp` with `want_response`).
    pub reply_with_nodeinfo: bool,
    /// Send an additional `NodeInfo` `FromRadio` to BLE after a NodeDB update.
    pub notify_ble_of_node_update: bool,
    /// Buffer this frame in NVS ring when BLE is offline (TextMessage only).
    pub buffer_if_offline: bool,
    /// Routing ACK: clear this `request_id` from `pending_acks`. `None` = no-op.
    pub clear_ack_id: Option<u32>,
    /// Admin packet addressed to us — `mesh_task` must call `handle_admin_from_ble`.
    pub admin_for_us: bool,
}

impl Default for RadioResult {
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

// ── Dispatch ──────────────────────────────────────────────────────────────────

pub fn dispatch(ctx: &RadioContext<'_>, node_db: &mut NodeDB) -> RadioResult {
    match PortNum::try_from(ctx.portnum).ok() {
        Some(PortNum::RemoteHardwareApp) => remote_hardware::handle(ctx.sender, ctx.payload),

        Some(PortNum::TextMessageApp) => text_message::handle(ctx.sender, ctx.payload),

        Some(PortNum::TextMessageCompressedApp) => {
            // N1: phone app decompresses; we forward as-is (no Unishox2 on device)
            text_message::handle(ctx.sender, ctx.payload)
        }

        Some(PortNum::PositionApp) => position::handle(ctx.sender, ctx.payload, node_db),

        Some(PortNum::NodeinfoApp) => {
            node_info::handle(ctx.sender, ctx.payload, ctx.want_response, node_db)
        }

        Some(PortNum::RoutingApp) => routing::handle(ctx.sender, ctx.payload, ctx.request_id),

        Some(PortNum::AdminApp) => RadioResult {
            // Admin packets addressed to us are handled locally by mesh_task.
            // Admin packets for others are forwarded to BLE as normal.
            forward_to_ble: !ctx.addressed_to_us,
            admin_for_us: ctx.addressed_to_us,
            ..RadioResult::default()
        },

        Some(PortNum::WaypointApp) => waypoint::handle(ctx.sender, ctx.payload),

        Some(PortNum::TelemetryApp) => telemetry::handle(ctx.sender, ctx.payload),

        Some(PortNum::TracerouteApp) => traceroute::handle(ctx.sender, ctx.payload),

        Some(PortNum::NeighborinfoApp) => neighbor_info::handle(ctx.sender, ctx.payload),

        _ => {
            warn!(
                "[PortHandler] Unknown portnum {} from {:08x}",
                ctx.portnum, ctx.sender
            );
            RadioResult::default()
        }
    }
}
