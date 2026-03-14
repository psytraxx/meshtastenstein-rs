//! Meshtastic channel configuration and PSK management

use crate::constants::{DEFAULT_PSK, MAX_CHANNELS};

/// A Meshtastic channel
#[derive(Debug, Clone)]
pub struct ChannelConfig {
    /// Channel index (0-7)
    pub index: u8,
    /// Channel name (empty = default)
    pub name: heapless::String<12>,
    /// Pre-shared key (16 or 32 bytes, empty = no encryption)
    pub psk: heapless::Vec<u8, 32>,
    /// Channel role
    pub role: ChannelRole,
}

/// Channel role
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChannelRole {
    Disabled = 0,
    Primary = 1,
    Secondary = 2,
}

impl ChannelConfig {
    /// Create the default primary channel with default PSK
    pub fn default_primary() -> Self {
        let mut psk = heapless::Vec::new();
        psk.extend_from_slice(&DEFAULT_PSK).ok();
        Self {
            index: 0,
            name: heapless::String::new(),
            psk,
            role: ChannelRole::Primary,
        }
    }

    /// Calculate the channel hash for quick packet matching.
    /// Used in the OTA header channel_index field for fast packet rejection.
    /// Algorithm: XOR all channel name bytes, then XOR all effective PSK bytes.
    /// This matches the official Meshtastic firmware `channelHash()` implementation.
    pub fn hash(&self) -> u8 {
        let mut h: u8 = 0;
        for &b in self.name.as_bytes() {
            h ^= b;
        }
        for &b in self.effective_psk() {
            h ^= b;
        }
        h
    }

    /// Get the effective PSK (returns default if PSK is the single-byte [0x01] sentinel)
    pub fn effective_psk(&self) -> &[u8] {
        if self.psk.len() == 1 && self.psk[0] == 0x01 {
            &DEFAULT_PSK
        } else {
            &self.psk
        }
    }

    /// Check if encryption is enabled for this channel
    pub fn is_encrypted(&self) -> bool {
        !self.psk.is_empty()
    }
}

/// Channel set: up to 8 channels
pub struct ChannelSet {
    channels: [Option<ChannelConfig>; MAX_CHANNELS],
}

impl ChannelSet {
    pub fn new() -> Self {
        let mut channels: [Option<ChannelConfig>; MAX_CHANNELS] = Default::default();
        channels[0] = Some(ChannelConfig::default_primary());
        Self { channels }
    }

    /// Get channel by index
    pub fn get(&self, index: u8) -> Option<&ChannelConfig> {
        self.channels.get(index as usize).and_then(|c| c.as_ref())
    }

    /// Get mutable channel by index
    pub fn get_mut(&mut self, index: u8) -> Option<&mut ChannelConfig> {
        self.channels
            .get_mut(index as usize)
            .and_then(|c| c.as_mut())
    }

    /// Set a channel at given index
    pub fn set(&mut self, index: u8, config: ChannelConfig) {
        if (index as usize) < MAX_CHANNELS {
            self.channels[index as usize] = Some(config);
        }
    }

    /// Find channel by hash value
    pub fn find_by_hash(&self, hash: u8) -> Option<&ChannelConfig> {
        self.channels
            .iter()
            .flatten()
            .find(|c| c.role != ChannelRole::Disabled && c.hash() == hash)
    }

    /// Get the primary channel
    pub fn primary(&self) -> Option<&ChannelConfig> {
        self.channels
            .iter()
            .flatten()
            .find(|c| c.role == ChannelRole::Primary)
    }

    /// Iterate over active channels
    pub fn active_channels(&self) -> impl Iterator<Item = &ChannelConfig> {
        self.channels
            .iter()
            .flatten()
            .filter(|c| c.role != ChannelRole::Disabled)
    }
}

impl Default for ChannelSet {
    fn default() -> Self {
        Self::new()
    }
}
