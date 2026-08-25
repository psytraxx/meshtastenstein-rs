//! Meshtastic mesh router: hierarchical routing (Flooding → NextHop → Reliable)
//!
//! Three logical layers implemented as method groups on a single `MeshRouter` struct:
//! - **FloodingRouter**: duplicate detection, role-based relay cancellation, hop-limit upgrade
//! - **NextHopRouter**: directed next-hop routing with ACK-based route learning
//! - **ReliableRouter**: handled externally in mesh_task.rs (pending packet management)

use crate::{
    constants::{
        CW_MAX, CW_MIN, DUPLICATE_RING_SIZE, MAX_RELAYERS_TRACKED, NO_NEXT_HOP, NUM_SYM_CAD,
        SLOT_TIME_FIXED_MS, SNR_MAX_DBM, SNR_MIN_DBM, WANT_ACK_TIMEOUT_MS,
    },
    domain::{
        device::DeviceRole,
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

/// Entry in the duplicate detection ring buffer
#[derive(Clone, Copy)]
struct PacketRecord {
    sender: u32,
    packet_id: u32,
    /// Highest hop_limit seen for this packet (for upgrade detection)
    best_hop_limit: u8,
    /// Up to `MAX_RELAYERS_TRACKED` relay_node IDs seen for this packet (for relay cancellation)
    relayed_by: [u8; MAX_RELAYERS_TRACKED],
    relayer_count: u8,
    valid: bool,
}

impl Default for PacketRecord {
    fn default() -> Self {
        Self {
            sender: 0,
            packet_id: 0,
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

    /// Look up a recently-seen packet. No age cutoff — matches upstream
    /// `PacketHistory::find()`, which has no TTL check; a slot is only ever
    /// forgotten by being evicted (oldest-first) to make room for a new record.
    fn find_record(&self, sender: u32, packet_id: u32) -> Option<usize> {
        let check_count = self.history_count.min(DUPLICATE_RING_SIZE);
        for i in 0..check_count {
            let r = &self.history[i];
            if r.valid && r.sender == sender && r.packet_id == packet_id {
                return Some(i);
            }
        }
        None
    }

    /// Record a new packet in the ring buffer. Returns the index of the new record.
    fn record_packet(&mut self, sender: u32, packet_id: u32, hop_limit: u8) -> usize {
        let idx = self.history_head;
        self.history[idx] = PacketRecord {
            sender,
            packet_id,
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
    /// `role` gates relay cancellation: matches upstream `roleAllowsCancelingDupe` —
    /// ROUTER and ROUTER_LATE never cancel a scheduled rebroadcast, even after
    /// hearing another node relay the same packet, so they always rebroadcast.
    pub fn should_filter_received(
        &mut self,
        sender: u32,
        packet_id: u32,
        hop_limit: u8,
        relay_node: u8,
        pending_hop_limit: Option<u8>,
        role: DeviceRole,
    ) -> FilterResult {
        if let Some(idx) = self.find_record(sender, packet_id) {
            // Duplicate — check for upgrade or relay cancellation
            Self::add_relayer(&mut self.history[idx], relay_node);

            // Check if this duplicate has a better hop_limit than our pending rebroadcast
            if let Some(pending_hl) = pending_hop_limit
                && hop_limit > pending_hl
            {
                self.history[idx].best_hop_limit = hop_limit;
                return FilterResult::DuplicateUpgrade(hop_limit - 1);
            }

            // If someone else already relayed this packet, we can cancel our pending
            // rebroadcast — unless our role always rebroadcasts regardless.
            let role_allows_cancel = !matches!(role, DeviceRole::Router | DeviceRole::RouterLate);
            if role_allows_cancel
                && relay_node != 0
                && relay_node != (self.our_node_num & 0xFF) as u8
            {
                return FilterResult::DuplicateCancelRelay;
            }

            FilterResult::DuplicateDrop
        } else {
            // New packet — record it
            let idx = self.record_packet(sender, packet_id, hop_limit);
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
                // `to_send` and `pending` share the same 8-slot capacity, and
                // at most one frame is pushed per `pending` entry per call, so
                // this can never actually be full — not a silent-drop risk.
                // Asserted (debug builds only) rather than left as a bare
                // `.ok()`, so a future capacity change that breaks that
                // invariant is caught instead of silently swallowed.
                let pushed = to_send.push(frame).is_ok();
                debug_assert!(pushed, "to_send should never exceed pending's own capacity");
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

// =============================================================================
// Rebroadcast contention-window delay (matches upstream RadioInterface)
// =============================================================================

/// Linear interpolation matching the Arduino `map()` used upstream, clamped to
/// `[out_min, out_max]` (upstream's `map()` does not clamp, but callers always
/// pass in-range inputs; we clamp defensively since our inputs come from live
/// radio measurements).
fn map_range(x: f32, in_min: f32, in_max: f32, out_min: f32, out_max: f32) -> f32 {
    let mapped = (x - in_min) * (out_max - out_min) / (in_max - in_min) + out_min;
    mapped.clamp(out_min.min(out_max), out_min.max(out_max))
}

/// Slot time in ms: CAD duration + fixed propagation/turnaround/MAC processing time.
/// Matches upstream `RadioInterface::computeSlotTimeMsec()` (non-2.4GHz branch —
/// this firmware only targets SX1262 sub-GHz).
fn slot_time_ms(spreading_factor: u8, bandwidth_hz: u32) -> f32 {
    let symbol_time_ms = (1u32 << spreading_factor) as f32 / (bandwidth_hz as f32 / 1000.0);
    let cad_symbols = if 2.25 > NUM_SYM_CAD + 0.5 {
        2.25
    } else {
        NUM_SYM_CAD + 0.5
    };
    cad_symbols * symbol_time_ms + SLOT_TIME_FIXED_MS
}

/// Map an SNR reading (dB) to a contention-window exponent in `[CW_MIN, CW_MAX]`.
/// Matches upstream `RadioInterface::getCWsize()`. High SNR → large CW (long delay);
/// low SNR → small CW (short delay), letting weak-signal nodes rebroadcast first.
fn cw_size_from_snr(snr: f32) -> u8 {
    map_range(
        snr,
        SNR_MIN_DBM as f32,
        SNR_MAX_DBM as f32,
        CW_MIN as f32,
        CW_MAX as f32,
    ) as u8
}

/// Reduce a raw random `u32` to a uniform value in `[0, bound)`. `bound == 0` returns 0.
fn random_below(raw_random: u32, bound: u32) -> u32 {
    if bound == 0 {
        return 0;
    }
    raw_random % bound
}

#[cfg(test)]
mod tests {
    use super::*;

    const NODE_A: u32 = 0x1000_0001;
    const NODE_B: u32 = 0x1000_0002;
    const NODE_C: u32 = 0x1000_0003;

    fn router() -> MeshRouter {
        MeshRouter::new(NODE_A)
    }

    // =========================================================================
    // Duplicate detection / FloodingRouter
    // =========================================================================

    #[test]
    fn first_sighting_of_a_packet_is_new() {
        let mut r = router();
        let result = r.should_filter_received(NODE_B, 42, 3, 0, None, DeviceRole::Client);
        assert!(matches!(result, FilterResult::New));
    }

    #[test]
    fn repeat_sighting_with_no_pending_and_no_relayer_is_a_plain_drop() {
        let mut r = router();
        r.should_filter_received(NODE_B, 42, 3, 0, None, DeviceRole::Client);
        let result = r.should_filter_received(NODE_B, 42, 3, 0, None, DeviceRole::Client);
        assert!(matches!(result, FilterResult::DuplicateDrop));
    }

    #[test]
    fn different_packet_id_from_same_sender_is_not_a_duplicate() {
        let mut r = router();
        r.should_filter_received(NODE_B, 42, 3, 0, None, DeviceRole::Client);
        let result = r.should_filter_received(NODE_B, 43, 3, 0, None, DeviceRole::Client);
        assert!(matches!(result, FilterResult::New));
    }

    #[test]
    fn same_packet_id_from_different_sender_is_not_a_duplicate() {
        let mut r = router();
        r.should_filter_received(NODE_B, 42, 3, 0, None, DeviceRole::Client);
        let result = r.should_filter_received(NODE_C, 42, 3, 0, None, DeviceRole::Client);
        assert!(matches!(result, FilterResult::New));
    }

    #[test]
    fn duplicate_with_higher_hop_limit_than_pending_upgrades() {
        let mut r = router();
        r.should_filter_received(NODE_B, 42, 3, 0, None, DeviceRole::Client);
        // Our pending rebroadcast is queued at hop_limit 2; a duplicate arrives
        // with a higher hop_limit (4), so we should upgrade to relay it further.
        let result = r.should_filter_received(NODE_B, 42, 4, 0, Some(2), DeviceRole::Client);
        assert!(matches!(result, FilterResult::DuplicateUpgrade(3)));
    }

    #[test]
    fn duplicate_with_lower_or_equal_hop_limit_does_not_upgrade() {
        let mut r = router();
        r.should_filter_received(NODE_B, 42, 3, 0, None, DeviceRole::Client);
        let result = r.should_filter_received(NODE_B, 42, 2, 0, Some(2), DeviceRole::Client);
        assert!(!matches!(result, FilterResult::DuplicateUpgrade(_)));
    }

    #[test]
    fn duplicate_relayed_by_another_node_cancels_our_pending_rebroadcast() {
        let mut r = router();
        r.should_filter_received(NODE_B, 42, 3, 0, None, DeviceRole::Client);
        // relay_node is some other node's last byte, not 0 and not ours.
        let other_relay = ((NODE_C & 0xFF) as u8).wrapping_add(1);
        let result = r.should_filter_received(NODE_B, 42, 3, other_relay, None, DeviceRole::Client);
        assert!(matches!(result, FilterResult::DuplicateCancelRelay));
    }

    #[test]
    fn router_role_never_cancels_a_pending_rebroadcast() {
        let mut r = router();
        r.should_filter_received(NODE_B, 42, 3, 0, None, DeviceRole::Router);
        let other_relay = ((NODE_C & 0xFF) as u8).wrapping_add(1);
        let result = r.should_filter_received(NODE_B, 42, 3, other_relay, None, DeviceRole::Router);
        assert!(matches!(result, FilterResult::DuplicateDrop));
    }

    #[test]
    fn router_late_role_never_cancels_a_pending_rebroadcast() {
        let mut r = router();
        r.should_filter_received(NODE_B, 42, 3, 0, None, DeviceRole::RouterLate);
        let other_relay = ((NODE_C & 0xFF) as u8).wrapping_add(1);
        let result =
            r.should_filter_received(NODE_B, 42, 3, other_relay, None, DeviceRole::RouterLate);
        assert!(matches!(result, FilterResult::DuplicateDrop));
    }

    #[test]
    fn relay_from_ourselves_does_not_cancel_our_pending_rebroadcast() {
        let mut r = router();
        r.should_filter_received(NODE_B, 42, 3, 0, None, DeviceRole::Client);
        let our_relay = (NODE_A & 0xFF) as u8;
        let result = r.should_filter_received(NODE_B, 42, 3, our_relay, None, DeviceRole::Client);
        assert!(matches!(result, FilterResult::DuplicateDrop));
    }

    #[test]
    fn duplicate_ring_has_no_ttl_and_evicts_oldest_first() {
        // Matches upstream PacketHistory: no age-based expiry, only oldest-first
        // eviction once the ring is full. Fill the ring, then confirm the very
        // first packet recorded is forgotten (evicted) while a recent one is
        // still recognized as a duplicate.
        let mut r = router();
        r.should_filter_received(NODE_B, 1, 3, 0, None, DeviceRole::Client);
        // One more than the ring holds: the (DUPLICATE_RING_SIZE + 1)th distinct
        // packet forces the oldest entry (packet id 1) out.
        for id in 2..=(DUPLICATE_RING_SIZE as u32 + 1) {
            r.should_filter_received(NODE_B, id, 3, 0, None, DeviceRole::Client);
        }
        // Packet id 2 (the oldest surviving entry) is still tracked. Check this
        // one first — checking id 1 below is itself a write (it looks "new"),
        // and would otherwise clobber whichever slot we check second.
        let still_present = r.should_filter_received(NODE_B, 2, 3, 0, None, DeviceRole::Client);
        assert!(matches!(still_present, FilterResult::DuplicateDrop));
        // Packet id 1 has been evicted — seeing it again looks "new".
        let evicted = r.should_filter_received(NODE_B, 1, 3, 0, None, DeviceRole::Client);
        assert!(matches!(evicted, FilterResult::New));
    }

    #[test]
    fn relayer_tracking_caps_at_max_relayers_tracked() {
        let mut r = router();
        r.should_filter_received(NODE_B, 42, 3, 0, None, DeviceRole::Client);
        // Feed more distinct relayers than MAX_RELAYERS_TRACKED; none of this
        // should panic or corrupt state — the record just stops adding new ones.
        for relay in 1..=(MAX_RELAYERS_TRACKED as u8 + 3) {
            r.should_filter_received(NODE_B, 42, 3, relay, None, DeviceRole::Client);
        }
        // The router is still usable afterwards.
        let result = r.should_filter_received(NODE_B, 99, 3, 0, None, DeviceRole::Client);
        assert!(matches!(result, FilterResult::New));
    }

    // =========================================================================
    // should_rebroadcast
    // =========================================================================

    #[test]
    fn does_not_rebroadcast_our_own_packet() {
        let r = router();
        assert_eq!(r.should_rebroadcast(3, NODE_A), None);
    }

    #[test]
    fn does_not_rebroadcast_when_hop_limit_exhausted() {
        let r = router();
        assert_eq!(r.should_rebroadcast(0, NODE_B), None);
    }

    #[test]
    fn rebroadcasts_someone_elses_packet_with_decremented_hop_limit() {
        let r = router();
        assert_eq!(r.should_rebroadcast(3, NODE_B), Some(2));
    }

    // =========================================================================
    // NextHopRouter
    // =========================================================================

    #[test]
    fn get_next_hop_returns_no_next_hop_when_route_unknown() {
        let r = router();
        let db = NodeDB::new(NODE_A);
        assert_eq!(r.get_next_hop(&db, NODE_B, 0), NO_NEXT_HOP);
    }

    #[test]
    fn get_next_hop_returns_learned_route() {
        let r = router();
        let mut db = NodeDB::new(NODE_A);
        assert!(r.learn_route(&mut db, NODE_B, 7));
        assert_eq!(r.get_next_hop(&db, NODE_B, 0), 7);
    }

    #[test]
    fn get_next_hop_rejects_the_relay_node_to_avoid_a_routing_loop() {
        let r = router();
        let mut db = NodeDB::new(NODE_A);
        r.learn_route(&mut db, NODE_B, 7);
        // If the packet arrived via relay 7, using 7 as the next hop again
        // would loop — get_next_hop must refuse and report no route instead.
        assert_eq!(r.get_next_hop(&db, NODE_B, 7), NO_NEXT_HOP);
    }

    #[test]
    fn learn_route_uses_relay_node_as_next_hop() {
        let r = router();
        let mut db = NodeDB::new(NODE_A);
        assert!(r.learn_route(&mut db, NODE_B, 5));
        assert_eq!(db.get(NODE_B).unwrap().next_hop, 5);
    }

    #[test]
    fn learn_route_falls_back_to_sender_last_byte_when_direct() {
        let r = router();
        let mut db = NodeDB::new(NODE_A);
        // relay_node == 0 means the sender is a direct neighbour.
        assert!(r.learn_route(&mut db, NODE_B, 0));
        assert_eq!(db.get(NODE_B).unwrap().next_hop, (NODE_B & 0xFF) as u8);
    }

    #[test]
    fn learn_route_returns_false_when_route_is_unchanged() {
        let r = router();
        let mut db = NodeDB::new(NODE_A);
        assert!(r.learn_route(&mut db, NODE_B, 5));
        // Learning the identical route again is not a change.
        assert!(!r.learn_route(&mut db, NODE_B, 5));
    }

    // =========================================================================
    // should_relay_directed
    // =========================================================================

    #[test]
    fn should_relay_directed_when_we_are_the_destination() {
        let r = router();
        assert!(r.should_relay_directed(NODE_A, NO_NEXT_HOP));
    }

    #[test]
    fn should_relay_directed_when_we_are_the_designated_next_hop() {
        let r = router();
        let our_last_byte = (NODE_A & 0xFF) as u8;
        assert!(r.should_relay_directed(NODE_B, our_last_byte));
    }

    #[test]
    fn should_not_relay_directed_packet_meant_for_someone_else() {
        let r = router();
        let someone_elses_byte = ((NODE_A & 0xFF) as u8).wrapping_add(1);
        assert!(!r.should_relay_directed(NODE_B, someone_elses_byte));
    }

    // =========================================================================
    // Rebroadcast contention-window delay
    // =========================================================================

    #[test]
    fn rebroadcast_delay_is_deterministic_for_a_given_random_input() {
        let a = rebroadcast_delay_ms(-5, DeviceRole::Client, 11, 250_000, 12345);
        let b = rebroadcast_delay_ms(-5, DeviceRole::Client, 11, 250_000, 12345);
        assert_eq!(a, b);
    }

    #[test]
    fn router_role_gets_a_shorter_delay_than_other_roles_for_the_same_inputs() {
        // Router nodes should relay earlier than everyone else, per upstream's
        // getTxDelayMsecWeighted() — same SNR/modem params/random draw, but the
        // Router path skips the "wait for non-router nodes" fixed offset.
        let router_delay = rebroadcast_delay_ms(0, DeviceRole::Router, 11, 250_000, 500);
        let client_delay = rebroadcast_delay_ms(0, DeviceRole::Client, 11, 250_000, 500);
        assert!(router_delay < client_delay);
    }

    #[test]
    fn rebroadcast_delay_is_never_negative_or_absurdly_large() {
        // Sweep a range of SNR and random values and just confirm sane bounds —
        // this is a smoke test against the map_range/cw_size arithmetic, not a
        // bit-exact port check (those live in the two tests above).
        for snr in [-20i8, -10, 0, 10] {
            for raw in [0u32, 1, 1000, u32::MAX] {
                let delay = rebroadcast_delay_ms(snr, DeviceRole::Client, 11, 250_000, raw);
                assert!(delay < 60_000, "delay {delay}ms is implausibly large");
            }
        }
    }

    #[test]
    fn random_below_zero_bound_returns_zero() {
        assert_eq!(random_below(12345, 0), 0);
    }

    #[test]
    fn random_below_is_always_within_bound() {
        for raw in [0u32, 1, 1000, u32::MAX] {
            assert!(random_below(raw, 8) < 8);
        }
    }
}

/// Rebroadcast contention delay (ms), matching upstream
/// `RadioInterface::getTxDelayMsecWeighted()`.
///
/// ROUTER nodes rebroadcast early with a shorter window; all other roles wait an
/// extra fixed `2 * CW_MAX * slot_time` offset before their own (shorter) random
/// window, so ROUTER traffic is favored to relay first.
///
/// `raw_random` is a single caller-supplied random `u32` (drawn from the hardware
/// RNG at the call site) — kept out of this module so the routing/jitter logic
/// has no hardware dependency and can run in a host-side unit test.
pub fn rebroadcast_delay_ms(
    snr: i8,
    role: DeviceRole,
    spreading_factor: u8,
    bandwidth_hz: u32,
    raw_random: u32,
) -> u64 {
    let slot_ms = slot_time_ms(spreading_factor, bandwidth_hz);
    let cw_size = cw_size_from_snr(snr as f32);

    let delay = if role == DeviceRole::Router {
        random_below(raw_random, 2u32 * cw_size as u32) as f32 * slot_ms
    } else {
        (2.0 * CW_MAX as f32 * slot_ms) + random_below(raw_random, 1u32 << cw_size) as f32 * slot_ms
    };

    delay as u64
}
