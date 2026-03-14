//! Dispatch incoming decoded mesh packets by PortNum

use crate::mesh::node_db::{NodeDB, NodePosition, NodeUser};
use crate::proto::PortNum;
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
    const TEXT_MESSAGE: u32 = PortNum::TextMessageApp as u32;
    const POSITION: u32 = PortNum::PositionApp as u32;
    const NODEINFO: u32 = PortNum::NodeinfoApp as u32;
    const ROUTING: u32 = PortNum::RoutingApp as u32;
    const ADMIN: u32 = PortNum::AdminApp as u32;
    const TELEMETRY: u32 = PortNum::TelemetryApp as u32;
    const TRACEROUTE: u32 = PortNum::TracerouteApp as u32;
    const NEIGHBORINFO: u32 = PortNum::NeighborinfoApp as u32;

    match portnum {
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
            if let Some(pos) = decode_position(payload)
                && let Some(node) = node_db.get_or_create(sender)
            {
                node.position = Some(pos);
                info!(
                    "[PortHandler] Updated position for {:08x}: lat={} lon={} alt={}",
                    sender,
                    pos.latitude_i as f64 / 1e7,
                    pos.longitude_i as f64 / 1e7,
                    pos.altitude
                );
            }
            HandleResult::Handled
        }

        NODEINFO => {
            debug!(
                "[PortHandler] NODEINFO from {:08x}: {} bytes",
                sender,
                payload.len()
            );
            if let Some(user) = decode_user(payload)
                && let Some(node) = node_db.get_or_create(sender)
            {
                info!(
                    "[PortHandler] Updated user for {:08x}: {} ({})",
                    sender,
                    user.long_name.as_str(),
                    user.short_name.as_str()
                );
                node.user = Some(user);
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

/// Minimal protobuf decode for Position message
fn decode_position(data: &[u8]) -> Option<NodePosition> {
    let mut pos = NodePosition {
        latitude_i: 0,
        longitude_i: 0,
        altitude: 0,
        time: 0,
    };

    let mut i = 0;
    while i < data.len() {
        let tag_byte = data[i];
        i += 1;
        let field_num = tag_byte >> 3;
        let wire_type = tag_byte & 0x07;

        match (field_num, wire_type) {
            // latitude_i: sfixed32 (field 1, wire type 5)
            (1, 5) if i + 4 <= data.len() => {
                pos.latitude_i =
                    i32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
                i += 4;
            }
            // longitude_i: sfixed32 (field 2, wire type 5)
            (2, 5) if i + 4 <= data.len() => {
                pos.longitude_i =
                    i32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
                i += 4;
            }
            // altitude: int32 (field 3, wire type 0)
            (3, 0) => {
                let (val, consumed) = decode_varint(&data[i..])?;
                pos.altitude = val as i32;
                i += consumed;
            }
            // time: fixed32 (field 4, wire type 5)
            (4, 5) if i + 4 <= data.len() => {
                pos.time = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
                i += 4;
            }
            // Skip unknown fields
            (_, 0) => {
                let (_, consumed) = decode_varint(&data[i..])?;
                i += consumed;
            }
            (_, 1) => i += 8,
            (_, 2) => {
                let (len, consumed) = decode_varint(&data[i..])?;
                i += consumed + len as usize;
            }
            (_, 5) => i += 4,
            _ => return None,
        }
    }

    Some(pos)
}

/// Minimal protobuf decode for User message
fn decode_user(data: &[u8]) -> Option<NodeUser> {
    let mut user = NodeUser {
        long_name: heapless::String::new(),
        short_name: heapless::String::new(),
        hw_model: 0,
        mac_addr: [0u8; 6],
    };

    let mut i = 0;
    while i < data.len() {
        if i >= data.len() {
            break;
        }
        let tag_byte = data[i];
        i += 1;
        let field_num = tag_byte >> 3;
        let wire_type = tag_byte & 0x07;

        match (field_num, wire_type) {
            // id: string (field 1, wire type 2)
            (1, 2) => {
                let (len, consumed) = decode_varint(&data[i..])?;
                i += consumed;
                i += len as usize; // skip ID string
            }
            // long_name: string (field 2, wire type 2)
            (2, 2) => {
                let (len, consumed) = decode_varint(&data[i..])?;
                i += consumed;
                let end = i + len as usize;
                if end <= data.len()
                    && let Ok(s) = core::str::from_utf8(&data[i..end])
                {
                    let _ = user.long_name.push_str(s);
                }
                i = end;
            }
            // short_name: string (field 3, wire type 2)
            (3, 2) => {
                let (len, consumed) = decode_varint(&data[i..])?;
                i += consumed;
                let end = i + len as usize;
                if end <= data.len()
                    && let Ok(s) = core::str::from_utf8(&data[i..end])
                {
                    let _ = user.short_name.push_str(s);
                }
                i = end;
            }
            // macaddr: bytes (field 4, wire type 2)
            (4, 2) => {
                let (len, consumed) = decode_varint(&data[i..])?;
                i += consumed;
                let end = i + len as usize;
                if end <= data.len() && len as usize == 6 {
                    user.mac_addr.copy_from_slice(&data[i..end]);
                }
                i = end;
            }
            // hw_model: enum (field 5, wire type 0)
            (5, 0) => {
                let (val, consumed) = decode_varint(&data[i..])?;
                user.hw_model = val as u32;
                i += consumed;
            }
            // Skip unknown fields
            (_, 0) => {
                let (_, consumed) = decode_varint(&data[i..])?;
                i += consumed;
            }
            (_, 1) => i += 8,
            (_, 2) => {
                let (len, consumed) = decode_varint(&data[i..])?;
                i += consumed + len as usize;
            }
            (_, 5) => i += 4,
            _ => return None,
        }
    }

    Some(user)
}

/// Decode a protobuf varint, returning (value, bytes_consumed)
fn decode_varint(data: &[u8]) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;
    for (i, &byte) in data.iter().enumerate() {
        result |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    None
}
