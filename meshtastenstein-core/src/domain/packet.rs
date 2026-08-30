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
    /// Extract hop_limit from flags (bits 2:0)
    pub fn hop_limit(&self) -> u8 {
        (self.flags >> FLAGS_HOP_LIMIT_SHIFT) & FLAGS_HOP_LIMIT_MASK
    }

    /// Extract hop_start from flags (bits 7:5)
    pub fn hop_start(&self) -> u8 {
        (self.flags >> FLAGS_HOP_START_SHIFT) & FLAGS_HOP_START_MASK
    }

    /// Extract want_ack from flags (bit 3)
    pub fn want_ack(&self) -> bool {
        self.flags & FLAGS_WANT_ACK_BIT != 0
    }

    /// Extract via_mqtt from flags (bit 4)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_header() -> PacketHeader {
        PacketHeader {
            destination: 0xAABB_CCDD,
            sender: 0x1122_3344,
            packet_id: 0xDEAD_BEEF,
            flags: PacketHeader::make_flags(true, false, 5, 3),
            channel_index: 2,
            next_hop: 0,
            relay_node: 0,
        }
    }

    // =========================================================================
    // Flags byte bit-packing — the highest-risk surface: a shift/mask bug here
    // silently corrupts hop-count and want_ack handling for every packet.
    // =========================================================================

    #[test]
    fn make_flags_round_trips_through_all_accessors() {
        let flags = PacketHeader::make_flags(true, true, 6, 7);
        let hdr = PacketHeader {
            destination: 0,
            sender: 0,
            packet_id: 0,
            flags,
            channel_index: 0,
            next_hop: 0,
            relay_node: 0,
        };
        assert!(hdr.want_ack());
        assert!(hdr.via_mqtt());
        assert_eq!(hdr.hop_limit(), 6);
        assert_eq!(hdr.hop_start(), 7);
    }

    #[test]
    fn hop_limit_and_hop_start_do_not_alias_each_other() {
        // hop_limit lives in bits 2:0, hop_start in bits 7:5 — a shift-amount
        // typo would make one field bleed into the other.
        let flags = PacketHeader::make_flags(false, false, 7, 0);
        let hdr = PacketHeader {
            destination: 0,
            sender: 0,
            packet_id: 0,
            flags,
            channel_index: 0,
            next_hop: 0,
            relay_node: 0,
        };
        assert_eq!(hdr.hop_limit(), 7);
        assert_eq!(hdr.hop_start(), 0);

        let flags = PacketHeader::make_flags(false, false, 0, 7);
        let hdr = PacketHeader { flags, ..hdr };
        assert_eq!(hdr.hop_limit(), 0);
        assert_eq!(hdr.hop_start(), 7);
    }

    #[test]
    fn set_hop_limit_leaves_other_flag_bits_untouched() {
        let mut hdr = sample_header();
        let before = (hdr.want_ack(), hdr.via_mqtt(), hdr.hop_start());
        hdr.set_hop_limit(1);
        assert_eq!(hdr.hop_limit(), 1);
        assert_eq!((hdr.want_ack(), hdr.via_mqtt(), hdr.hop_start()), before);
    }

    #[test]
    fn hop_limit_and_hop_start_are_masked_to_three_bits() {
        // Fields are only 3 bits wide; a value with high bits set must not
        // corrupt neighbouring flag bits when packed.
        let flags = PacketHeader::make_flags(true, true, 0xFF, 0xFF);
        let hdr = PacketHeader {
            destination: 0,
            sender: 0,
            packet_id: 0,
            flags,
            channel_index: 0,
            next_hop: 0,
            relay_node: 0,
        };
        assert_eq!(hdr.hop_limit(), 0b111);
        assert_eq!(hdr.hop_start(), 0b111);
        assert!(hdr.want_ack());
        assert!(hdr.via_mqtt());
    }

    // =========================================================================
    // Header encode/decode round trip
    // =========================================================================

    #[test]
    fn header_round_trips_through_encode_decode() {
        let original = sample_header();
        let mut buf = [0u8; HEADER_SIZE];
        original.encode(&mut buf);
        let decoded = PacketHeader::decode(&buf).unwrap();

        assert_eq!(decoded.destination, original.destination);
        assert_eq!(decoded.sender, original.sender);
        assert_eq!(decoded.packet_id, original.packet_id);
        assert_eq!(decoded.flags, original.flags);
        assert_eq!(decoded.channel_index, original.channel_index);
        assert_eq!(decoded.next_hop, original.next_hop);
        assert_eq!(decoded.relay_node, original.relay_node);
    }

    #[test]
    fn decode_rejects_a_buffer_shorter_than_the_header() {
        let buf = [0u8; HEADER_SIZE - 1];
        assert!(PacketHeader::decode(&buf).is_none());
    }

    #[test]
    fn decode_accepts_a_buffer_longer_than_the_header() {
        // decode() only needs the first HEADER_SIZE bytes; callers pass a
        // whole RadioFrame buffer that includes the payload after it.
        let mut buf = [0u8; HEADER_SIZE + 10];
        let mut hdr_buf = [0u8; HEADER_SIZE];
        sample_header().encode(&mut hdr_buf);
        buf[..HEADER_SIZE].copy_from_slice(&hdr_buf);
        assert!(PacketHeader::decode(&buf).is_some());
    }

    #[test]
    fn is_for_us_matches_our_address_or_broadcast() {
        let mut hdr = sample_header();
        hdr.destination = 0x1234;
        assert!(hdr.is_for_us(0x1234));
        assert!(!hdr.is_for_us(0x5678));

        hdr.destination = BROADCAST_ADDR;
        assert!(hdr.is_for_us(0x1234));
        assert!(hdr.is_for_us(0x5678));
    }

    // =========================================================================
    // RadioFrame construction
    // =========================================================================

    #[test]
    fn from_parts_round_trips_header_and_payload() {
        let header = sample_header();
        let payload = [1u8, 2, 3, 4, 5];
        let frame = RadioFrame::from_parts(&header, &payload).unwrap();

        assert_eq!(frame.len, HEADER_SIZE + payload.len());
        assert_eq!(frame.payload(), &payload);
        let decoded = frame.header().unwrap();
        assert_eq!(decoded.destination, header.destination);
        assert_eq!(decoded.packet_id, header.packet_id);
    }

    #[test]
    fn from_parts_rejects_a_payload_that_would_overflow_the_frame() {
        let header = sample_header();
        let oversized = alloc::vec![0u8; MAX_LORA_PAYLOAD_LEN]; // + HEADER_SIZE overflows
        assert!(RadioFrame::from_parts(&header, &oversized).is_none());
    }

    #[test]
    fn from_raw_rejects_a_buffer_shorter_than_the_header() {
        let short = [0u8; HEADER_SIZE - 1];
        assert!(RadioFrame::from_raw(&short).is_none());
    }

    #[test]
    fn from_raw_rejects_a_buffer_longer_than_max_payload() {
        let oversized = alloc::vec![0u8; MAX_LORA_PAYLOAD_LEN + 1];
        assert!(RadioFrame::from_raw(&oversized).is_none());
    }

    #[test]
    fn from_raw_round_trips_via_as_bytes() {
        let header = sample_header();
        let payload = [9u8, 8, 7];
        let original = RadioFrame::from_parts(&header, &payload).unwrap();

        let reparsed = RadioFrame::from_raw(original.as_bytes()).unwrap();
        assert_eq!(reparsed.as_bytes(), original.as_bytes());
        assert_eq!(reparsed.payload(), &payload);
    }

    #[test]
    fn header_and_payload_return_empty_on_a_too_short_frame() {
        let mut frame = RadioFrame::new();
        frame.len = HEADER_SIZE - 1;
        assert!(frame.header().is_none());
        assert_eq!(frame.payload(), &[] as &[u8]);
    }

    #[test]
    fn with_rewritten_header_replaces_hop_limit_and_relay_node_only() {
        let header = sample_header();
        let frame = RadioFrame::from_parts(&header, &[1, 2, 3]).unwrap();

        let rewritten = frame.with_rewritten_header(1, 0x42);
        let hdr = rewritten.header().unwrap();

        assert_eq!(hdr.hop_limit(), 1);
        assert_eq!(hdr.relay_node, 0x42);
        // Everything else is unchanged.
        assert_eq!(hdr.destination, header.destination);
        assert_eq!(hdr.sender, header.sender);
        assert_eq!(hdr.packet_id, header.packet_id);
        assert_eq!(hdr.hop_start(), header.hop_start());
        assert_eq!(hdr.want_ack(), header.want_ack());
        // Payload is untouched too.
        assert_eq!(rewritten.payload(), frame.payload());
    }
}
