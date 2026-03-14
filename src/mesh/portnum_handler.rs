//! Dispatch incoming decoded mesh packets by PortNum

use crate::mesh::node_db::NodeDB;
use crate::proto::{PortNum, Position as ProtoPosition, User as ProtoUser};
use prost::Message;
use log::{debug, info, warn};

/// Result of handling a port-specific message
pub enum HandleResult {
    /// Message was handled, no further action needed
    Handled,
    /// Forward this text message to BLE
    TextMessage(alloc::boxed::Box<heapless::Vec<u8, 256>>),
    /// Not handled
    NotHandled,
}

/// Handle a decoded Data message based on its portnum
pub fn handle_portnum(
    portnum: u32,
    payload: &[u8],
    sender: u32,
    node_db: &mut NodeDB,
    _time_secs: u32,
) -> HandleResult {
    // Use proto PortNum enum as named constants
    const REMOTE_HARDWARE: u32 = PortNum::RemoteHardwareApp as u32;
    const TEXT_MESSAGE: u32 = PortNum::TextMessageApp as u32;
    const POSITION: u32 = PortNum::PositionApp as u32;
    const NODEINFO: u32 = PortNum::NodeinfoApp as u32;
    const ROUTING: u32 = PortNum::RoutingApp as u32;
    const ADMIN: u32 = PortNum::AdminApp as u32;
    const TEXT_MESSAGE_COMPRESSED: u32 = PortNum::TextMessageCompressedApp as u32;
    const WAYPOINT: u32 = PortNum::WaypointApp as u32;
    const TELEMETRY: u32 = PortNum::TelemetryApp as u32;
    const TRACEROUTE: u32 = PortNum::TracerouteApp as u32;
    const NEIGHBORINFO: u32 = PortNum::NeighborinfoApp as u32;

    match portnum {
        REMOTE_HARDWARE => {
            debug!(
                "[PortHandler] REMOTE_HARDWARE from {:08x}: {} bytes (forwarded to BLE)",
                sender,
                payload.len()
            );
            HandleResult::Handled
        }

        TEXT_MESSAGE => {
            info!(
                "[PortHandler] TEXT_MESSAGE from {:08x}: {} bytes",
                sender,
                payload.len()
            );
            let mut data = heapless::Vec::new();
            data.extend_from_slice(payload).ok();
            HandleResult::TextMessage(alloc::boxed::Box::new(data))
        }

        POSITION => {
            debug!(
                "[PortHandler] POSITION from {:08x}: {} bytes",
                sender,
                payload.len()
            );
            match ProtoPosition::decode(payload) {
                Ok(pos) => {
                    if let Some(node) = node_db.get_or_create(sender) {
                        info!(
                            "[PortHandler] Updated position for {:08x}: lat={} lon={} alt={}",
                            sender,
                            pos.latitude_i.unwrap_or(0) as f64 / 1e7,
                            pos.longitude_i.unwrap_or(0) as f64 / 1e7,
                            pos.altitude.unwrap_or(0)
                        );
                        node.position = Some(pos);
                    }
                }
                Err(e) => warn!("[PortHandler] POSITION decode failed from {:08x}: {:?}", sender, e),
            }
            HandleResult::Handled
        }

        NODEINFO => {
            debug!(
                "[PortHandler] NODEINFO from {:08x}: {} bytes",
                sender,
                payload.len()
            );
            match ProtoUser::decode(payload) {
                Ok(user) => {
                    if let Some(node) = node_db.get_or_create(sender) {
                        info!(
                            "[PortHandler] Updated user for {:08x}: {} ({})",
                            sender,
                            &user.long_name,
                            &user.short_name
                        );
                        node.user = Some(user);
                    }
                }
                Err(e) => warn!("[PortHandler] NODEINFO decode failed from {:08x}: {:?}", sender, e),
            }
            HandleResult::Handled
        }

        ROUTING => {
            debug!(
                "[PortHandler] ROUTING from {:08x}: {} bytes",
                sender,
                payload.len()
            );
            HandleResult::Handled
        }

        ADMIN => {
            debug!(
                "[PortHandler] ADMIN from {:08x}: {} bytes",
                sender,
                payload.len()
            );
            HandleResult::NotHandled
        }

        TEXT_MESSAGE_COMPRESSED => {
            // N1: phone app decompresses; we forward as-is (no Unishox2 on device)
            info!(
                "[PortHandler] TEXT_MESSAGE_COMPRESSED from {:08x}: {} bytes",
                sender,
                payload.len()
            );
            HandleResult::Handled
        }

        WAYPOINT => {
            // N2: forward to BLE; phone app renders waypoints
            debug!(
                "[PortHandler] WAYPOINT from {:08x}: {} bytes",
                sender,
                payload.len()
            );
            HandleResult::Handled
        }

        TELEMETRY => {
            debug!(
                "[PortHandler] TELEMETRY from {:08x}: {} bytes",
                sender,
                payload.len()
            );
            HandleResult::Handled
        }

        TRACEROUTE => {
            debug!(
                "[PortHandler] TRACEROUTE from {:08x}: {} bytes",
                sender,
                payload.len()
            );
            HandleResult::Handled
        }

        NEIGHBORINFO => {
            debug!(
                "[PortHandler] NEIGHBORINFO from {:08x}: {} bytes",
                sender,
                payload.len()
            );
            HandleResult::Handled
        }

        _ => {
            warn!(
                "[PortHandler] Unknown portnum {} from {:08x}",
                portnum, sender
            );
            HandleResult::NotHandled
        }
    }
}

