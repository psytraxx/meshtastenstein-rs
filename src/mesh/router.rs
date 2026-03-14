//! Meshtastic mesh router: duplicate detection, flooding, hop management

use crate::constants::DUPLICATE_RING_SIZE;
use embassy_time::Instant;

/// How long (milliseconds) a packet is considered "recently seen" for duplicate detection.
/// Delayed retransmissions beyond this window are treated as new packets.
const DUP_TTL_MS: u64 = 60 * 60 * 1_000; // 1 hour

/// Entry in the duplicate detection ring buffer
#[derive(Clone, Copy)]
struct DupEntry {
    sender: u32,
    packet_id: u32,
    seen_at: Instant,
    valid: bool,
}

impl Default for DupEntry {
    fn default() -> Self {
        Self {
            sender: 0,
            packet_id: 0,
            seen_at: Instant::MIN,
            valid: false,
        }
    }
}

/// Duplicate detection and flood routing state
pub struct MeshRouter {
    dup_ring: [DupEntry; DUPLICATE_RING_SIZE],
    dup_head: usize,
    dup_count: usize,
    our_node_num: u32,
}

impl MeshRouter {
    pub fn new(our_node_num: u32) -> Self {
        Self {
            dup_ring: [DupEntry::default(); DUPLICATE_RING_SIZE],
            dup_head: 0,
            dup_count: 0,
            our_node_num,
        }
    }

    pub fn our_node_num(&self) -> u32 {
        self.our_node_num
    }

    /// Check if a packet is a duplicate. If not, record it.
    /// Returns true if the packet was already seen within the TTL window (duplicate).
    /// Entries older than DUP_TTL_MS are considered expired and do not match.
    pub fn is_duplicate(&mut self, sender: u32, packet_id: u32) -> bool {
        let now = Instant::now();
        let ttl = embassy_time::Duration::from_millis(DUP_TTL_MS);

        // Check existing entries within TTL
        let check_count = self.dup_count.min(DUPLICATE_RING_SIZE);
        for i in 0..check_count {
            let entry = &self.dup_ring[i];
            if entry.valid
                && entry.sender == sender
                && entry.packet_id == packet_id
                && now.saturating_duration_since(entry.seen_at) <= ttl
            {
                return true;
            }
        }

        // Not a duplicate - record it
        self.dup_ring[self.dup_head] = DupEntry {
            sender,
            packet_id,
            seen_at: now,
            valid: true,
        };
        self.dup_head = (self.dup_head + 1) % DUPLICATE_RING_SIZE;
        if self.dup_count < DUPLICATE_RING_SIZE {
            self.dup_count += 1;
        }

        false
    }

    /// Determine if we should rebroadcast a packet.
    /// Returns the new hop_limit if we should rebroadcast, None otherwise.
    pub fn should_rebroadcast(&self, hop_limit: u8, sender: u32) -> Option<u8> {
        // Don't rebroadcast our own packets
        if sender == self.our_node_num {
            return None;
        }
        // Don't rebroadcast if hop limit exhausted
        if hop_limit == 0 {
            return None;
        }
        Some(hop_limit - 1)
    }

    /// Calculate SNR-based contention delay for rebroadcast (ms).
    /// Better SNR = longer delay (let weaker-signal nodes rebroadcast first).
    pub fn rebroadcast_delay_ms(&self, snr: i8) -> u64 {
        // Meshtastic uses a slot-based contention window
        // Higher SNR = later slot to let weaker signals relay first
        let base_delay: u64 = 100;
        let snr_factor = if snr > 0 { snr as u64 * 10 } else { 0 };
        base_delay + snr_factor
    }
}
