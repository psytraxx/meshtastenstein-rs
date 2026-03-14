//! Dispatch incoming decoded mesh packets by PortNum

use log::{debug, info, warn};

use crate::mesh::node_db::{NodeDB, NodePosition, NodeUser};

/// Meshtastic port numbers (from portnums.proto)
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u32)]
pub enum PortNum {
    Unknown = 0,
    TextMessage = 1,
    RemoteHardware = 2,
    Position = 3,
    NodeInfo = 4,
    Routing = 5,
    Admin = 6,
    TextMessageCompressed = 7,
    Waypoint = 8,
    Audio = 9,
    DetectionSensor = 10,
    Reply = 32,
    IpTunnel = 33,
    Paxcounter = 34,
    Serial = 64,
    StoreForward = 65,
    RangeTest = 66,
    Telemetry = 67,
    Zps = 68,
    Simulator = 69,
    Traceroute = 70,
    Neighborinfo = 71,
    Atak = 72,
    MapReport = 73,
    PowerStress = 74,
    Max = 256,
}

impl PortNum {
    pub fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::TextMessage,
            3 => Self::Position,
            4 => Self::NodeInfo,
            5 => Self::Routing,
            6 => Self::Admin,
            65 => Self::StoreForward,
            67 => Self::Telemetry,
            70 => Self::Traceroute,
            71 => Self::Neighborinfo,
            72 => Self::Atak,
            _ => Self::Unknown,
        }
    }
}

/// Result of handling a port-specific message
pub enum HandleResult {
    /// Message was handled, no further action needed
    Handled,
    /// Forward this text message to BLE
    TextMessage(alloc::boxed::Box<heapless::Vec<u8, 256>>),
    /// Send a routing ACK/NACK
    RoutingResponse(u32, u32, u8), // dest, request_id, error_code
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
    let port = PortNum::from_u32(portnum);

    match port {
        PortNum::TextMessage => {
            info!(
                "[PortHandler] TEXT_MESSAGE from {:08x}: {} bytes",
                sender,
                payload.len()
            );
            let mut data = heapless::Vec::new();
            data.extend_from_slice(payload).ok();
            HandleResult::TextMessage(alloc::boxed::Box::new(data))
        }

        PortNum::Position => {
            debug!(
                "[PortHandler] POSITION from {:08x}: {} bytes",
                sender,
                payload.len()
            );
            // Decode position protobuf - minimal inline decode
            // Position proto: latitude_i (sfixed32, field 1), longitude_i (sfixed32, field 2),
            //                 altitude (int32, field 3), time (fixed32, field 4)
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

        PortNum::NodeInfo => {
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

        PortNum::Routing => {
            debug!(
                "[PortHandler] ROUTING from {:08x}: {} bytes",
                sender,
                payload.len()
            );
            // Routing messages contain ACKs/NACKs
            HandleResult::Handled
        }

        PortNum::Admin => {
            debug!(
                "[PortHandler] ADMIN from {:08x}: {} bytes",
                sender,
                payload.len()
            );
            HandleResult::NotHandled
        }

        PortNum::Telemetry => {
            debug!(
                "[PortHandler] TELEMETRY from {:08x}: {} bytes",
                sender,
                payload.len()
            );
            HandleResult::Handled
        }

        PortNum::Traceroute => {
            debug!(
                "[PortHandler] TRACEROUTE from {:08x}: {} bytes",
                sender,
                payload.len()
            );
            // Traceroute packets are forwarded to BLE by mesh_task; no local action needed
            HandleResult::Handled
        }

        PortNum::Neighborinfo => {
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
