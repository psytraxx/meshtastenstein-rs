use crate::domain::packet::RadioFrame;
use embassy_time::Instant;

/// Pending rebroadcast
pub struct PendingRebroadcast {
    pub frame: RadioFrame,
    pub deadline: Instant,
}

/// Pending outgoing packet awaiting routing ACK (replaces PendingAck)
pub struct PendingPacket {
    pub frame: RadioFrame,
    pub packet_id: u32,
    pub dest: u32,
    /// Original sender (may differ from us for relayed packets)
    pub sender: u32,
    pub deadline: Instant,
    pub retries_left: u8,
    /// true = we originated this packet, false = we're relaying for someone else
    pub is_our_packet: bool,
}
