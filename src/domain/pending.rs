use crate::domain::packet::RadioFrame;
use embassy_time::Instant;

/// Pending rebroadcast
pub struct PendingRebroadcast {
    pub frame: RadioFrame,
    pub deadline: Instant,
}

/// Pending outgoing packet awaiting routing ACK (M1)
pub struct PendingAck {
    pub frame: RadioFrame,
    pub packet_id: u32,
    pub dest: u32,
    pub deadline: Instant,
    pub retries_left: u8,
}
