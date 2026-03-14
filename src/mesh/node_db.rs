//! NodeDB: tracks known nodes in the mesh network

use heapless::Vec;

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

    /// Get or create a node entry, returning a mutable reference
    pub fn get_or_create(&mut self, node_num: u32) -> Option<&mut NodeEntry> {
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

    /// Update last heard time and SNR for a node
    pub fn touch(&mut self, node_num: u32, time: u32, snr: i8) {
        if let Some(node) = self.get_or_create(node_num) {
            node.last_heard = time;
            node.snr = snr;
        }
    }
}
