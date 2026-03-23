//! Meshtastic mesh router: hierarchical routing (Flooding → NextHop → Reliable)
//!
//! Three logical layers implemented as method groups on a single `MeshRouter` struct:
//! - **FloodingRouter**: duplicate detection, role-based relay cancellation, hop-limit upgrade
//! - **NextHopRouter**: directed next-hop routing with ACK-based route learning
//! - **ReliableRouter**: handled externally in mesh_task.rs (pending packet management)

use crate::constants::{DUPLICATE_RING_SIZE, MAX_RELAYERS_TRACKED, NO_NEXT_HOP};
use crate::domain::node_db::NodeDB;
use crate::domain::packet::RadioFrame;
use embassy_time::Instant;
use log::info;

/// Pending rebroadcast scheduled by the FloodingRouter layer.
pub struct PendingRebroadcast {
    pub frame: RadioFrame,
    pub deadline: Instant,
}

/// Pending outgoing packet awaiting a routing ACK (ReliableRouter layer).
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

/// How long (milliseconds) a packet is considered "recently seen" for duplicate detection.
const DUP_TTL_MS: u64 = 60 * 60 * 1_000; // 1 hour

/// Entry in the duplicate detection ring buffer
#[derive(Clone, Copy)]
struct PacketRecord {
    sender: u32,
    packet_id: u32,
    seen_at_ms: u64,
    /// Highest hop_limit seen for this packet (for upgrade detection)
    best_hop_limit: u8,
    /// Up to 4 relay_node IDs seen for this packet (for relay cancellation)
    relayed_by: [u8; MAX_RELAYERS_TRACKED],
    relayer_count: u8,
    valid: bool,
}

impl Default for PacketRecord {
    fn default() -> Self {
        Self {
            sender: 0,
            packet_id: 0,
            seen_at_ms: 0,
            best_hop_limit: 0,
            relayed_by: [0; MAX_RELAYERS_TRACKED],
            relayer_count: 0,
            valid: false,
        }
    }
}

/// Result of the flooding-layer duplicate/filter check
pub enum FilterResult {
    /// First time seeing this packet — process normally
    New,
    /// Duplicate — drop silently
    DuplicateDrop,
    /// Duplicate with a better path — upgrade pending rebroadcast to this hop_limit
    DuplicateUpgrade(u8),
    /// Duplicate relayed by another node — cancel our pending rebroadcast
    DuplicateCancelRelay,
}

/// Duplicate detection and hierarchical routing state
pub struct MeshRouter {
    history: [PacketRecord; DUPLICATE_RING_SIZE],
    history_head: usize,
    history_count: usize,
    our_node_num: u32,
}

impl MeshRouter {
    pub fn new(our_node_num: u32) -> Self {
        Self {
            history: [PacketRecord::default(); DUPLICATE_RING_SIZE],
            history_head: 0,
            history_count: 0,
            our_node_num,
        }
    }

    pub fn our_node_num(&self) -> u32 {
        self.our_node_num
    }

    // =========================================================================
    // PacketHistory helpers
    // =========================================================================

    /// Look up a recently-seen packet. Returns the index if found within TTL.
    fn find_record(&self, sender: u32, packet_id: u32, now_ms: u64) -> Option<usize> {
        let check_count = self.history_count.min(DUPLICATE_RING_SIZE);
        for i in 0..check_count {
            let r = &self.history[i];
            if r.valid
                && r.sender == sender
                && r.packet_id == packet_id
                && now_ms.saturating_sub(r.seen_at_ms) <= DUP_TTL_MS
            {
                return Some(i);
            }
        }
        None
    }

    /// Record a new packet in the ring buffer. Returns the index of the new record.
    fn record_packet(&mut self, sender: u32, packet_id: u32, now_ms: u64, hop_limit: u8) -> usize {
        let idx = self.history_head;
        self.history[idx] = PacketRecord {
            sender,
            packet_id,
            seen_at_ms: now_ms,
            best_hop_limit: hop_limit,
            relayed_by: [0; MAX_RELAYERS_TRACKED],
            relayer_count: 0,
            valid: true,
        };
        self.history_head = (self.history_head + 1) % DUPLICATE_RING_SIZE;
        if self.history_count < DUPLICATE_RING_SIZE {
            self.history_count += 1;
        }
        idx
    }

    /// Add a relayer to a packet record (if not already tracked and space available)
    fn add_relayer(record: &mut PacketRecord, relay_node: u8) {
        if relay_node == 0 || relay_node == NO_NEXT_HOP {
            return;
        }
        let count = record.relayer_count as usize;
        // Check if already tracked
        for i in 0..count.min(MAX_RELAYERS_TRACKED) {
            if record.relayed_by[i] == relay_node {
                return;
            }
        }
        if count < MAX_RELAYERS_TRACKED {
            record.relayed_by[count] = relay_node;
            record.relayer_count += 1;
        }
    }

    // =========================================================================
    // FloodingRouter layer
    // =========================================================================

    /// Flooding-layer filter for received packets.
    ///
    /// Handles: duplicate detection, hop-limit upgrade, role-based relay cancellation.
    /// `relay_node` is from the OTA header (byte 15).
    /// `pending_hop_limit` is the hop_limit of our pending rebroadcast for this packet (if any).
    pub fn should_filter_received(
        &mut self,
        sender: u32,
        packet_id: u32,
        hop_limit: u8,
        relay_node: u8,
        now_ms: u64,
        pending_hop_limit: Option<u8>,
    ) -> FilterResult {
        if let Some(idx) = self.find_record(sender, packet_id, now_ms) {
            // Duplicate — check for upgrade or relay cancellation
            Self::add_relayer(&mut self.history[idx], relay_node);

            // Check if this duplicate has a better hop_limit than our pending rebroadcast
            if let Some(pending_hl) = pending_hop_limit
                && hop_limit > pending_hl
            {
                self.history[idx].best_hop_limit = hop_limit;
                return FilterResult::DuplicateUpgrade(hop_limit - 1);
            }

            // If someone else already relayed this packet, we can cancel our pending rebroadcast
            if relay_node != 0 && relay_node != (self.our_node_num & 0xFF) as u8 {
                return FilterResult::DuplicateCancelRelay;
            }

            FilterResult::DuplicateDrop
        } else {
            // New packet — record it
            let idx = self.record_packet(sender, packet_id, now_ms, hop_limit);
            Self::add_relayer(&mut self.history[idx], relay_node);
            FilterResult::New
        }
    }

    /// Determine if we should rebroadcast a packet (flooding layer).
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
        let base_delay: u64 = 100;
        let snr_factor = if snr > 0 { snr as u64 * 10 } else { 0 };
        base_delay + snr_factor
    }

    // =========================================================================
    // NextHopRouter layer
    // =========================================================================

    /// Look up the next-hop for a destination node.
    /// Returns NO_NEXT_HOP (0) if no route is known.
    /// Avoids routing loops by rejecting relay_node as next_hop.
    pub fn get_next_hop(&self, node_db: &NodeDB, dest: u32, relay_node: u8) -> u8 {
        if let Some(entry) = node_db.get(dest) {
            let nh = entry.next_hop;
            if nh != NO_NEXT_HOP && nh != relay_node {
                return nh;
            }
        }
        NO_NEXT_HOP
    }

    /// Learn a route from a routing ACK.
    ///
    /// When we receive an ACK from `from` (forwarded via `relay_node`), we know that
    /// `relay_node` is a valid next-hop to reach `from`. Updates NodeDB accordingly.
    ///
    /// Returns true if a route was learned or updated.
    pub fn learn_route(&self, node_db: &mut NodeDB, from: u32, relay_node: u8) -> bool {
        // Use relay_node as next_hop; if 0, the sender is a direct neighbour.
        let next_hop = if relay_node != 0 {
            relay_node
        } else {
            (from & 0xFF) as u8
        };

        if next_hop == NO_NEXT_HOP {
            return false;
        }

        if let Some(entry) = node_db.get_or_create(from)
            && entry.next_hop != next_hop
        {
            info!(
                "[Router] Update next hop of {:08x} to 0x{:02x}",
                from, next_hop
            );
            entry.next_hop = next_hop;
            return true;
        }
        false
    }

    /// Check if we should relay a directed (non-broadcast) packet.
    /// Returns true if:
    /// - The packet's destination is us (we need to process it), OR
    /// - We are the designated next_hop for this packet
    pub fn should_relay_directed(&self, dest: u32, next_hop: u8) -> bool {
        if dest == self.our_node_num {
            return true;
        }
        // We're the designated relay if next_hop matches our node's last byte
        let our_last_byte = (self.our_node_num & 0xFF) as u8;
        next_hop != NO_NEXT_HOP && next_hop == our_last_byte
    }
}
