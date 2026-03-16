//! NodeDB: tracks known nodes in the mesh network

use heapless::Vec;
use log::warn;

use crate::constants::MAX_NODES;
use crate::proto::{Position as ProtoPosition, User as ProtoUser};

/// Information about a node in the mesh
#[derive(Clone)]
pub struct NodeEntry {
    pub node_num: u32,
    pub user: Option<ProtoUser>,
    pub position: Option<ProtoPosition>,
    pub last_heard: u32, // epoch seconds
    pub snr: i8,
    pub hops_away: u8,
    /// Monotonic boot-relative timestamp (ms) of last reception from this node.
    /// Used for online_count() congestion scaling.
    pub last_seen_ms: u64,
}

/// Database of known mesh nodes
pub struct NodeDB {
    nodes: Vec<NodeEntry, MAX_NODES>,
    our_node_num: u32,
}

impl NodeDB {
    pub fn new(our_node_num: u32) -> Self {
        Self {
            nodes: Vec::new(),
            our_node_num,
        }
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
        if node_num == 0x0000_0000 || node_num == 0xFFFF_FFFF {
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
            last_seen_ms: 0,
        };

        if self.nodes.push(entry).is_ok() {
            let idx = self.nodes.len() - 1;
            Some(&mut self.nodes[idx])
        } else {
            None // DB full
        }
    }

    /// Get a node entry by node number
    pub fn get(&self, node_num: u32) -> Option<&NodeEntry> {
        self.nodes.iter().find(|n| n.node_num == node_num)
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
}
