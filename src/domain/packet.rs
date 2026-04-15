//! Meshtastic OTA packet header and radio frame encoding/decoding
//!
//! The 16-byte OTA header format:
//! ```text
//! Offset  Size  Field
//! 0       4     destination (u32 LE)
//! 4       4     sender (u32 LE)
//! 8       4     packet_id (u32 LE)
//! 12      1     flags: hop_limit[2:0](3) | want_ack[3](1) | via_mqtt[4](1) | hop_start[7:5](3)
//! 13      1     channel_index
//! 14      1     next_hop (reserved, usually 0)
//! 15      1     relay_node (reserved, usually 0)
//! ```

use crate::constants::MAX_LORA_PAYLOAD_LEN;

/// Size of the Meshtastic OTA packet header
pub const HEADER_SIZE: usize = 16;

/// Broadcast address
pub const BROADCAST_ADDR: u32 = 0xFFFFFFFF;

// ---- Flags byte bit-field constants ----
// Official Meshtastic OTA flags layout:
//   Bits [2:0] = hop_limit (3 bits)
//   Bit  [3]   = want_ack
//   Bit  [4]   = via_mqtt
//   Bits [7:5] = hop_start (3 bits)
/// Bits 2:0 — hop_limit field shift (3 bits)
const FLAGS_HOP_LIMIT_SHIFT: u8 = 0;
/// Hop limit field mask (after shift)
const FLAGS_HOP_LIMIT_MASK: u8 = 0x07;
/// Clear mask for hop_limit bits in flags (bits 2:0 = 0b00000111)
const FLAGS_HOP_LIMIT_CLEAR: u8 = 0b1111_1000;
/// Bit 3: want_ack flag
const FLAGS_WANT_ACK_BIT: u8 = 0x08;
/// Bit 4: via_mqtt flag
const FLAGS_VIA_MQTT_BIT: u8 = 0x10;
/// Bits 7:5 — hop_start field shift (3 bits)
const FLAGS_HOP_START_SHIFT: u8 = 5;
/// Hop start field mask (after shift)
const FLAGS_HOP_START_MASK: u8 = 0x07;

/// Parsed Meshtastic OTA header
#[derive(Debug, Clone, Copy)]
pub struct PacketHeader {
    pub destination: u32,
    pub sender: u32,
    pub packet_id: u32,
    pub flags: u8,
    pub channel_index: u8,
    pub next_hop: u8,
    pub relay_node: u8,
}

impl PacketHeader {
    /// Extract hop_limit from flags (bits 4:2)
    pub fn hop_limit(&self) -> u8 {
        (self.flags >> FLAGS_HOP_LIMIT_SHIFT) & FLAGS_HOP_LIMIT_MASK
    }

    /// Extract hop_start from flags (bits 7:5)
    pub fn hop_start(&self) -> u8 {
        (self.flags >> FLAGS_HOP_START_SHIFT) & FLAGS_HOP_START_MASK
    }

    /// Extract want_ack from flags (bit 0)
    pub fn want_ack(&self) -> bool {
        self.flags & FLAGS_WANT_ACK_BIT != 0
    }

    /// Extract via_mqtt from flags (bit 1)
    pub fn via_mqtt(&self) -> bool {
        self.flags & FLAGS_VIA_MQTT_BIT != 0
    }

    /// Set hop_limit in flags (bits 4:2), leaving other bits unchanged
    pub fn set_hop_limit(&mut self, limit: u8) {
        self.flags = (self.flags & FLAGS_HOP_LIMIT_CLEAR)
            | ((limit & FLAGS_HOP_LIMIT_MASK) << FLAGS_HOP_LIMIT_SHIFT);
    }

    /// Build flags byte from components
    pub fn make_flags(want_ack: bool, via_mqtt: bool, hop_limit: u8, hop_start: u8) -> u8 {
        let mut f: u8 = 0;
        if want_ack {
            f |= FLAGS_WANT_ACK_BIT;
        }
        if via_mqtt {
            f |= FLAGS_VIA_MQTT_BIT;
        }
        f |= (hop_limit & FLAGS_HOP_LIMIT_MASK) << FLAGS_HOP_LIMIT_SHIFT;
        f |= (hop_start & FLAGS_HOP_START_MASK) << FLAGS_HOP_START_SHIFT;
        f
    }

    /// Encode header into 16-byte buffer
    pub fn encode(&self, buf: &mut [u8; HEADER_SIZE]) {
        buf[0..4].copy_from_slice(&self.destination.to_le_bytes());
        buf[4..8].copy_from_slice(&self.sender.to_le_bytes());
        buf[8..12].copy_from_slice(&self.packet_id.to_le_bytes());
        buf[12] = self.flags;
        buf[13] = self.channel_index;
        buf[14] = self.next_hop;
        buf[15] = self.relay_node;
    }

    /// Decode header from 16-byte buffer
    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < HEADER_SIZE {
            return None;
        }
        Some(Self {
            destination: u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
            sender: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            packet_id: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
            flags: buf[12],
            channel_index: buf[13],
            next_hop: buf[14],
            relay_node: buf[15],
        })
    }

    /// Check if this packet is addressed to us or broadcast
    pub fn is_for_us(&self, our_node_num: u32) -> bool {
        self.destination == our_node_num || self.destination == BROADCAST_ADDR
    }
}

/// A complete radio frame: header + encrypted payload
#[derive(Clone)]
pub struct RadioFrame {
    pub data: [u8; MAX_LORA_PAYLOAD_LEN],
    pub len: usize,
}

impl RadioFrame {
    pub fn new() -> Self {
        Self {
            data: [0u8; MAX_LORA_PAYLOAD_LEN],
            len: 0,
        }
    }

    /// Get the header portion
    pub fn header(&self) -> Option<PacketHeader> {
        if self.len >= HEADER_SIZE {
            PacketHeader::decode(&self.data)
        } else {
            None
        }
    }

    /// Get the encrypted payload portion (after header)
    pub fn payload(&self) -> &[u8] {
        if self.len > HEADER_SIZE {
            &self.data[HEADER_SIZE..self.len]
        } else {
            &[]
        }
    }

    /// Create a frame from header + encrypted payload
    pub fn from_parts(header: &PacketHeader, encrypted_payload: &[u8]) -> Option<Self> {
        let total_len = HEADER_SIZE + encrypted_payload.len();
        if total_len > MAX_LORA_PAYLOAD_LEN {
            return None;
        }
        let mut frame = Self::new();
        let mut hdr_buf = [0u8; HEADER_SIZE];
        header.encode(&mut hdr_buf);
        frame.data[..HEADER_SIZE].copy_from_slice(&hdr_buf);
        frame.data[HEADER_SIZE..total_len].copy_from_slice(encrypted_payload);
        frame.len = total_len;
        Some(frame)
    }

    /// Create a frame from raw received bytes
    pub fn from_raw(data: &[u8]) -> Option<Self> {
        if data.len() < HEADER_SIZE || data.len() > MAX_LORA_PAYLOAD_LEN {
            return None;
        }
        let mut frame = Self::new();
        frame.data[..data.len()].copy_from_slice(data);
        frame.len = data.len();
        Some(frame)
    }

    /// Get raw bytes for transmission
    pub fn as_bytes(&self) -> &[u8] {
        &self.data[..self.len]
    }

    /// Return a clone with the header's hop_limit and relay_node replaced.
    /// Used by the flooding router when scheduling or upgrading rebroadcasts.
    pub fn with_rewritten_header(&self, hop_limit: u8, relay_node: u8) -> Self {
        let mut frame = self.clone();
        if let Some(mut hdr) = frame.header() {
            hdr.set_hop_limit(hop_limit);
            hdr.relay_node = relay_node;
            let mut buf = [0u8; HEADER_SIZE];
            hdr.encode(&mut buf);
            frame.data[..HEADER_SIZE].copy_from_slice(&buf);
        }
        frame
    }
}

impl Default for RadioFrame {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for RadioFrame {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if let Some(hdr) = self.header() {
            write!(
                f,
                "RadioFrame(to={:08x} from={:08x} id={:08x} ch={} hop={}/{} payload={}B)",
                hdr.destination,
                hdr.sender,
                hdr.packet_id,
                hdr.channel_index,
                hdr.hop_limit(),
                hdr.hop_start(),
                self.len.saturating_sub(HEADER_SIZE)
            )
        } else {
            write!(f, "RadioFrame({}B, invalid)", self.len)
        }
    }
}
