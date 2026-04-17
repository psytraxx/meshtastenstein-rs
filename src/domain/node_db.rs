//! NodeDB: tracks known nodes in the mesh network

use heapless::Vec;
use log::{info, warn};

use crate::{
    constants::MAX_NODES,
    domain::packet::BROADCAST_ADDR,
    proto::{Position as ProtoPosition, User as ProtoUser},
};

/// Maximum number of node records persisted to flash.
/// v2 records are 96 bytes each; 42 records + 16-byte header = 4048 bytes,
/// fitting within one 4 KB NVS sector.
pub const MAX_PERSISTED_NODES: usize = 42;

/// Bytes per node in the persisted snapshot.
/// v2 layout (96 bytes):
///   0..4   node_num
///   4..8   last_heard
///   8..16  last_seen_ms
///  16      snr
///  17      hops_away
///  18      next_hop
///  19      reserved
///  20      short_name_len
///  21..26  short_name (5 bytes)
///  26      long_name_len
///  27..55  long_name (28 bytes)
///  55      role
///  56      hw_model_low
///  57..64  reserved
///  64..96  X25519 peer public key (32 bytes; all-zero = not known)
pub const SNAPSHOT_RECORD_SIZE: usize = 96;

/// Snapshot header (16 bytes): magic (4) + version (1) + count (1) + reserved (10).
pub const SNAPSHOT_HEADER_SIZE: usize = 16;

/// Total bytes of a fully-packed snapshot.
pub const SNAPSHOT_BYTES: usize = SNAPSHOT_HEADER_SIZE + MAX_PERSISTED_NODES * SNAPSHOT_RECORD_SIZE;

const SNAPSHOT_MAGIC: u32 = 0x4E444232; // "NDB2"
const SNAPSHOT_VERSION: u8 = 2;

/// Information about a node in the mesh
#[derive(Clone)]
pub struct NodeEntry {
    pub node_num: u32,
    pub user: Option<ProtoUser>,
    pub position: Option<ProtoPosition>,
    pub last_heard: u32, // epoch seconds
    pub snr: i8,
    pub hops_away: u8,
    /// Last byte of preferred relay node for reaching this node (0 = unknown)
    pub next_hop: u8,
    /// Monotonic boot-relative timestamp (ms) of last reception from this node.
    /// Used for online_count() congestion scaling.
    pub last_seen_ms: u64,
    /// X25519 public key (32 bytes) extracted from the peer's NodeInfo `User`
    /// proto. `None` until a NodeInfo with a populated `public_key` field is
    /// received. Not persisted in the v1 snapshot — re-learned after reboot.
    pub pub_key: Option<[u8; 32]>,
}

/// Database of known mesh nodes
pub struct NodeDB {
    nodes: Vec<NodeEntry, MAX_NODES>,
    our_node_num: u32,
    /// Set whenever a record is added or mutated; cleared after a successful
    /// flush by the orchestrator. Lets the persistence layer batch writes.
    dirty: bool,
}

impl NodeDB {
    pub fn new(our_node_num: u32) -> Self {
        Self {
            nodes: Vec::new(),
            our_node_num,
            dirty: false,
        }
    }

    /// True when in-memory state diverges from the last persisted snapshot.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Mark clean — called by the orchestrator after a successful flush.
    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn our_node_num(&self) -> u32 {
        self.our_node_num
    }

    /// Remove nodes not heard from within the last `max_age_secs` seconds.
    /// Preserves our own entry. Returns the number of entries removed.
    pub fn prune_stale(&mut self, now_secs: u32, max_age_secs: u32) -> usize {
        let before = self.nodes.len();
        let our = self.our_node_num;
        self.nodes.retain(|n| {
            n.node_num == our
                || n.last_heard == 0
                || now_secs.saturating_sub(n.last_heard) <= max_age_secs
        });
        before - self.nodes.len()
    }

    /// Get or create a node entry, returning a mutable reference.
    /// Rejects reserved node numbers (0x00000000 broadcast/invalid, 0xFFFFFFFF broadcast).
    pub fn get_or_create(&mut self, node_num: u32) -> Option<&mut NodeEntry> {
        // Reject reserved node numbers
        if node_num == 0x0000_0000 || node_num == BROADCAST_ADDR {
            return None;
        }

        // Find existing
        if let Some(idx) = self.nodes.iter().position(|n| n.node_num == node_num) {
            return Some(&mut self.nodes[idx]);
        }

        // Create new if space available
        let entry = NodeEntry {
            node_num,
            user: None,
            position: None,
            last_heard: 0,
            snr: 0,
            hops_away: 0,
            next_hop: 0,
            last_seen_ms: 0,
            pub_key: None,
        };

        if self.nodes.push(entry).is_ok() {
            let idx = self.nodes.len() - 1;
            self.dirty = true;
            Some(&mut self.nodes[idx])
        } else {
            None // DB full
        }
    }

    pub fn update_user(&mut self, node_num: u32, user: ProtoUser) {
        if let Some(node) = self.get_or_create(node_num) {
            node.user = Some(user);
            self.mark_dirty();
        }
    }

    /// Store a peer's X25519 public key. Once known, the orchestrator can
    /// prefer PKC encryption for direct messages to that peer.
    /// Marks dirty so the orchestrator knows to flush on the next interval.
    pub fn update_pub_key(&mut self, node_num: u32, key: [u8; 32]) {
        if let Some(node) = self.get_or_create(node_num) {
            node.pub_key = Some(key);
            self.mark_dirty();
        }
    }

    pub fn update_position(&mut self, node_num: u32, position: ProtoPosition) {
        if let Some(node) = self.get_or_create(node_num) {
            node.position = Some(position);
            // Position changes are noisy; do NOT mark dirty here. The position
            // is non-essential for cold-boot routing and would cause excessive
            // flash wear if every position broadcast triggered a flush.
        }
    }

    /// Get a node entry by node number
    pub fn get(&self, node_num: u32) -> Option<&NodeEntry> {
        self.nodes.iter().find(|n| n.node_num == node_num)
    }

    /// Return `true` if `node_num` is in the DB and has a stored X25519 public key.
    pub fn has_pub_key(db: &NodeDB, node_num: u32) -> bool {
        db.get(node_num).and_then(|e| e.pub_key).is_some()
    }

    /// Get a mutable node entry by node number
    pub fn get_mut(&mut self, node_num: u32) -> Option<&mut NodeEntry> {
        self.nodes.iter_mut().find(|n| n.node_num == node_num)
    }

    /// Iterate all nodes
    pub fn iter(&self) -> impl Iterator<Item = &NodeEntry> {
        self.nodes.iter()
    }

    /// Number of known nodes
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Remove a node by node number. Returns true if the node was found and removed.
    pub fn remove(&mut self, node_num: u32) -> bool {
        if let Some(idx) = self.nodes.iter().position(|n| n.node_num == node_num) {
            self.nodes.swap_remove(idx);
            true
        } else {
            false
        }
    }

    /// Update last heard time and SNR for a node.
    /// If the DB is full, prunes nodes not heard from in 2+ hours before inserting.
    /// `now_ms` is monotonic milliseconds since boot (used for congestion scaling).
    pub fn touch(&mut self, node_num: u32, time: u32, snr: i8, now_ms: u64) {
        // If full and this is a new node, prune stale entries first
        if self.nodes.is_full() && self.nodes.iter().all(|n| n.node_num != node_num) {
            const STALE_AGE_SECS: u32 = 2 * 60 * 60; // 2 hours
            let pruned = self.prune_stale(time, STALE_AGE_SECS);
            if pruned > 0 {
                warn!("[NodeDB] Pruned {} stale node(s) to make room", pruned);
            } else {
                warn!(
                    "[NodeDB] DB full ({} nodes), new node {:08x} dropped",
                    MAX_NODES, node_num
                );
                return;
            }
        }
        if let Some(node) = self.get_or_create(node_num) {
            node.last_heard = time;
            node.snr = snr;
            node.last_seen_ms = now_ms;
        }
    }

    /// Count nodes heard within the last `max_age_ms` milliseconds (monotonic).
    /// Used for congestion scaling.
    pub fn online_count(&self, now_ms: u64, max_age_ms: u64) -> usize {
        self.nodes
            .iter()
            .filter(|n| n.last_seen_ms > 0 && now_ms.saturating_sub(n.last_seen_ms) <= max_age_ms)
            .count()
    }

    /// Serialize the most-recently-heard `MAX_PERSISTED_NODES` entries into a
    /// fixed-size flash-friendly snapshot. Always returns exactly
    /// `SNAPSHOT_BYTES` bytes; unused records are zero-filled.
    pub fn to_snapshot(&self) -> [u8; SNAPSHOT_BYTES] {
        let mut buf = [0u8; SNAPSHOT_BYTES];

        // Build an index list sorted by last_heard descending. We keep the top
        // MAX_PERSISTED_NODES — the rest are forgotten across reboots, which
        // is fine because the worst case is that they re-announce on first
        // contact after boot.
        let mut order: heapless::Vec<usize, MAX_NODES> = heapless::Vec::new();
        for i in 0..self.nodes.len() {
            let _ = order.push(i);
        }
        order.sort_unstable_by_key(|&i| core::cmp::Reverse(self.nodes[i].last_heard));
        let count = order.len().min(MAX_PERSISTED_NODES) as u8;

        // Header
        buf[0..4].copy_from_slice(&SNAPSHOT_MAGIC.to_le_bytes());
        buf[4] = SNAPSHOT_VERSION;
        buf[5] = count;
        // bytes 6..16 reserved

        // Records
        for (slot, &node_idx) in order.iter().take(count as usize).enumerate() {
            let off = SNAPSHOT_HEADER_SIZE + slot * SNAPSHOT_RECORD_SIZE;
            let n = &self.nodes[node_idx];
            encode_record(&mut buf[off..off + SNAPSHOT_RECORD_SIZE], n);
        }
        buf
    }

    /// Restore from a snapshot. Replaces all entries except `our_node_num`.
    /// Silently ignores corrupt or wrong-version blobs.
    pub fn restore_snapshot(&mut self, buf: &[u8]) -> bool {
        if buf.len() < SNAPSHOT_BYTES {
            return false;
        }
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != SNAPSHOT_MAGIC || buf[4] != SNAPSHOT_VERSION {
            info!(
                "[NodeDB] Snapshot format mismatch (magic={:#010x} ver={}), starting fresh",
                magic, buf[4]
            );
            return false;
        }
        let count = buf[5] as usize;
        if count > MAX_PERSISTED_NODES {
            return false;
        }

        // Preserve our own entry if it's already present.
        let our = self.our_node_num;
        self.nodes.retain(|n| n.node_num == our);

        for slot in 0..count {
            let off = SNAPSHOT_HEADER_SIZE + slot * SNAPSHOT_RECORD_SIZE;
            if let Some(entry) = decode_record(&buf[off..off + SNAPSHOT_RECORD_SIZE])
                && entry.node_num != our
                && self.nodes.iter().all(|n| n.node_num != entry.node_num)
                && self.nodes.push(entry).is_err()
            {
                break;
            }
        }
        // Restored state matches what's on disk → not dirty.
        self.dirty = false;
        true
    }
}

fn encode_record(buf: &mut [u8], n: &NodeEntry) {
    debug_assert!(buf.len() >= SNAPSHOT_RECORD_SIZE);
    buf[0..4].copy_from_slice(&n.node_num.to_le_bytes());
    buf[4..8].copy_from_slice(&n.last_heard.to_le_bytes());
    buf[8..16].copy_from_slice(&n.last_seen_ms.to_le_bytes());
    buf[16] = n.snr as u8;
    buf[17] = n.hops_away;
    buf[18] = n.next_hop;
    // byte 19 reserved

    // Names: short (5) + long (28). Anything longer is truncated; the next
    // NodeInfo broadcast from that peer will rehydrate the full string.
    let (sn, sn_len) = encode_str_field(n.user.as_ref().map(|u| u.short_name.as_str()), 5);
    let (ln, ln_len) = encode_str_field(n.user.as_ref().map(|u| u.long_name.as_str()), 28);
    buf[20] = sn_len;
    buf[21..26].copy_from_slice(&sn[..5]);
    buf[26] = ln_len;
    buf[27..55].copy_from_slice(&ln[..28]);

    // Role / hw_model_low for cheap reconstruction of the User proto.
    buf[55] = n.user.as_ref().map(|u| u.role as u8).unwrap_or(0);
    buf[56] = n
        .user
        .as_ref()
        .map(|u| (u.hw_model as u32 & 0xFF) as u8)
        .unwrap_or(0);
    // bytes 57..64 reserved

    // v2: X25519 peer public key at bytes 64..96; all-zero means not known.
    if let Some(key) = n.pub_key {
        buf[64..96].copy_from_slice(&key);
    }
}

fn decode_record(buf: &[u8]) -> Option<NodeEntry> {
    debug_assert!(buf.len() >= SNAPSHOT_RECORD_SIZE);
    let node_num = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if node_num == 0 || node_num == BROADCAST_ADDR {
        return None;
    }
    let last_heard = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let last_seen_ms = u64::from_le_bytes([
        buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
    ]);
    let snr = buf[16] as i8;
    let hops_away = buf[17];
    let next_hop = buf[18];

    let sn_len = buf[20].min(5) as usize;
    let ln_len = buf[26].min(28) as usize;
    let short_name = core::str::from_utf8(&buf[21..21 + sn_len])
        .ok()
        .map(alloc::string::ToString::to_string);
    let long_name = core::str::from_utf8(&buf[27..27 + ln_len])
        .ok()
        .map(alloc::string::ToString::to_string);
    let role = buf[55] as i32;
    let hw_model = buf[56] as i32;

    let user = if short_name.is_some() || long_name.is_some() {
        Some(ProtoUser {
            id: alloc::string::String::new(),
            long_name: long_name.unwrap_or_default(),
            short_name: short_name.unwrap_or_default(),
            hw_model,
            role,
            ..Default::default()
        })
    } else {
        None
    };

    // v2: X25519 peer public key at bytes 64..96; all-zero means not stored.
    let pub_key = if buf[64..96].iter().any(|&b| b != 0) {
        let mut key = [0u8; 32];
        key.copy_from_slice(&buf[64..96]);
        Some(key)
    } else {
        None
    };

    Some(NodeEntry {
        node_num,
        user,
        position: None,
        last_heard,
        snr,
        hops_away,
        next_hop,
        last_seen_ms,
        pub_key,
    })
}

fn encode_str_field(src: Option<&str>, max_len: usize) -> ([u8; 28], u8) {
    let mut out = [0u8; 28];
    let bytes = src.map(str::as_bytes).unwrap_or(&[]);
    let n = bytes.len().min(max_len);
    out[..n].copy_from_slice(&bytes[..n]);
    (out, n as u8)
}

extern crate alloc;
