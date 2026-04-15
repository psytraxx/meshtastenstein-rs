//! Meshtastic mesh router: hierarchical routing (Flooding → NextHop → Reliable)
//!
//! Three logical layers implemented as method groups on a single `MeshRouter` struct:
//! - **FloodingRouter**: duplicate detection, role-based relay cancellation, hop-limit upgrade
//! - **NextHopRouter**: directed next-hop routing with ACK-based route learning
//! - **ReliableRouter**: handled externally in mesh_task.rs (pending packet management)

use crate::{
    constants::{DUPLICATE_RING_SIZE, MAX_RELAYERS_TRACKED, NO_NEXT_HOP, WANT_ACK_TIMEOUT_MS},
    domain::{
        node_db::NodeDB,
        packet::{HEADER_SIZE, RadioFrame},
    },
};
use embassy_time::{Duration, Instant};
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

    // =========================================================================
    // ReliableRouter layer
    // =========================================================================

    /// Extend all pending-packet deadlines by `extension`. Called when the channel
    /// is heard to be busy, so we avoid colliding with an ongoing transmission.
    pub fn extend_pending_deadlines(
        pending: &mut heapless::Vec<PendingPacket, 8>,
        extension: Duration,
    ) {
        for p in pending.iter_mut() {
            p.deadline += extension;
        }
    }

    /// Process timed-out `want_ack` packets. For each expired entry:
    /// - If retries remain: decrements the counter, falls back to flooding on the
    ///   last retry (clears `next_hop` in NodeDB and in the frame header), and
    ///   returns the frame to send.
    /// - If exhausted: removes the entry and logs a timeout.
    ///
    /// Returns the frames that should be (re)transmitted, in order.
    pub fn tick_retransmissions(
        &mut self,
        pending: &mut heapless::Vec<PendingPacket, 8>,
        node_db: &mut NodeDB,
    ) -> heapless::Vec<RadioFrame, 8> {
        let now = Instant::now();
        let mut to_send: heapless::Vec<RadioFrame, 8> = heapless::Vec::new();
        let mut i = 0;
        while i < pending.len() {
            if now < pending[i].deadline {
                i += 1;
                continue;
            }

            if pending[i].retries_left > 0 {
                let retries_left = pending[i].retries_left - 1;
                let packet_id = pending[i].packet_id;
                let dest = pending[i].dest;

                let frame = if retries_left == 0 {
                    // Last retry: fall back to flooding
                    info!(
                        "[Router] Last retry for {:08x} to {:08x}, falling back to flood",
                        packet_id, dest
                    );
                    if let Some(entry) = node_db.get_mut(dest) {
                        entry.next_hop = NO_NEXT_HOP;
                    }
                    // Clear next_hop in the frame header too
                    let mut frame = pending[i].frame.clone();
                    if let Some(mut hdr) = frame.header() {
                        hdr.next_hop = NO_NEXT_HOP;
                        let mut buf = [0u8; HEADER_SIZE];
                        hdr.encode(&mut buf);
                        frame.data[..HEADER_SIZE].copy_from_slice(&buf);
                    }
                    frame
                } else {
                    info!(
                        "[Router] Retransmitting {:08x} to {:08x} ({} retries left)",
                        packet_id, dest, retries_left
                    );
                    pending[i].frame.clone()
                };

                pending[i].frame = frame.clone();
                pending[i].deadline = Instant::now() + Duration::from_millis(WANT_ACK_TIMEOUT_MS);
                pending[i].retries_left = retries_left;
                to_send.push(frame).ok();
                i += 1;
            } else {
                let packet_id = pending[i].packet_id;
                let dest = pending[i].dest;
                info!(
                    "[Router] ACK timeout for {:08x} to {:08x}, giving up",
                    packet_id, dest
                );
                pending.swap_remove(i);
                // Don't increment i — the swapped element needs checking
            }
        }
        to_send
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
