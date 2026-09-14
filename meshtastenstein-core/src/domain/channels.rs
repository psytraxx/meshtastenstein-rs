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

/// Channel role — re-exported from proto to avoid duplication.
pub use crate::proto::channel::Role as ChannelRole;

impl ChannelConfig {
    /// Create the default primary channel with default PSK
    pub fn default_primary() -> Self {
        let mut psk = heapless::Vec::new();
        // Store the [0x01] sentinel so Android shows the yellow "default key" lock icon.
        // effective_psk() expands this to the full DEFAULT_PSK for cryptographic use.
        psk.push(0x01).ok();
        Self {
            index: 0,
            name: heapless::String::new(),
            psk,
            role: ChannelRole::Primary,
        }
    }

    /// Calculate the channel hash for OTA packet matching.
    pub fn hash(&self, preset_name: &str) -> u8 {
        let effective_name = if self.name.is_empty() {
            preset_name
        } else {
            self.name.as_str()
        };
        let mut h: u8 = 0;
        for &b in effective_name.as_bytes() {
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
#[cfg_attr(feature = "test-harness", derive(Clone))]
pub struct ChannelSet {
    pub(crate) channels: [Option<ChannelConfig>; MAX_CHANNELS],
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

    /// Find channel by hash value.
    pub fn find_by_hash(&self, hash: u8, preset_name: &str) -> Option<&ChannelConfig> {
        self.channels
            .iter()
            .flatten()
            .find(|c| c.role != ChannelRole::Disabled && c.hash(preset_name) == hash)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The `[0x01]` sentinel must hash as though it were the fully-expanded
    /// `DEFAULT_PSK`, not as the literal single byte `0x01` — this is the
    /// single gate every PSK-encrypted RX packet passes through
    /// (`ChannelSet::find_by_hash`), so a bug here doesn't fail loudly, it
    /// just makes the device deaf to the entire public (default-channel) mesh.
    #[test]
    fn default_primary_channel_hash_matches_an_independently_computed_value() {
        // XOR-fold of "LongFast" (0x4c^0x6f^0x6e^0x67^0x46^0x61^0x73^0x74)
        // and the real 16-byte DEFAULT_PSK, computed independently in Python.
        let ch = ChannelConfig::default_primary();
        assert_eq!(ch.hash("LongFast"), 0x08);
    }

    #[test]
    fn the_one_byte_sentinel_hashes_as_the_expanded_default_psk_not_as_0x01() {
        let ch = ChannelConfig::default_primary();
        assert_eq!(
            ch.psk.as_slice(),
            &[0x01],
            "sentinel must still be stored as-is"
        );
        assert_eq!(
            ch.effective_psk(),
            &DEFAULT_PSK,
            "but hashing/encryption must use the fully-expanded key"
        );
        // If hashing used the literal 1-byte PSK instead, this would differ
        // from the value computed against the real DEFAULT_PSK above.
        assert_eq!(ch.hash("LongFast"), 0x08);
    }

    #[test]
    fn empty_name_falls_back_to_the_preset_name_for_hashing() {
        let mut ch = ChannelConfig::default_primary();
        assert!(ch.name.is_empty());
        let via_empty_name = ch.hash("LongFast");

        ch.name = heapless::String::try_from("LongFast").unwrap();
        let via_explicit_name = ch.hash("ShortFast"); // preset_name should be ignored now

        assert_eq!(
            via_empty_name, via_explicit_name,
            "an empty channel name must hash identically to an explicit name equal to the preset"
        );
    }

    #[test]
    fn a_non_empty_name_is_used_verbatim_ignoring_the_preset_name() {
        let mut ch = ChannelConfig::default_primary();
        ch.name = heapless::String::try_from("mychan").unwrap();
        // Changing the preset name argument must not change the hash, since
        // the channel has its own explicit name.
        assert_eq!(ch.hash("LongFast"), ch.hash("ShortFast"));
    }

    #[test]
    fn find_by_hash_skips_disabled_channels() {
        let mut set = ChannelSet::new();
        let hash = set.primary().unwrap().hash("LongFast");

        // A disabled channel at another index with the SAME effective
        // name+PSK (so the same hash) must not be returned by find_by_hash.
        let mut disabled = ChannelConfig::default_primary();
        disabled.index = 1;
        disabled.role = ChannelRole::Disabled;
        set.set(1, disabled);

        let found = set
            .find_by_hash(hash, "LongFast")
            .expect("primary should still be found");
        assert_eq!(
            found.role,
            ChannelRole::Primary,
            "must resolve to the enabled channel, not the disabled one"
        );
    }

    #[test]
    fn find_by_hash_returns_none_when_no_channel_matches() {
        let set = ChannelSet::new();
        let real_hash = set.primary().unwrap().hash("LongFast");
        // Pick a hash value guaranteed not to match (XOR-fold is 8-bit, so
        // flipping the low bit changes the result).
        let wrong_hash = real_hash ^ 0x01;
        assert!(set.find_by_hash(wrong_hash, "LongFast").is_none());
    }

    #[test]
    fn find_by_hash_resolves_a_collision_to_the_first_matching_index_deterministically() {
        // Two distinct channels engineered to hash identically are, by
        // construction, indistinguishable from the wire byte alone — the
        // requirement is only that resolution is deterministic (always picks
        // the same one), not which one it picks.
        let mut set = ChannelSet::new();
        let mut secondary = ChannelConfig::default_primary();
        secondary.index = 1;
        secondary.role = ChannelRole::Secondary;
        // Identical name+psk as the primary => identical hash under any preset.
        set.set(1, secondary);

        let hash = set.primary().unwrap().hash("LongFast");
        let first = set.find_by_hash(hash, "LongFast").map(|c| c.index);
        let second = set.find_by_hash(hash, "LongFast").map(|c| c.index);
        assert_eq!(
            first, second,
            "repeated lookups of the same collision must agree"
        );
        assert_eq!(
            first,
            Some(0),
            "iteration order means index 0 (primary) wins ties"
        );
    }

    #[test]
    fn get_and_get_mut_return_none_for_an_empty_or_out_of_range_slot() {
        let mut set = ChannelSet::new();
        assert!(set.get(1).is_none(), "slot 1 starts unpopulated");
        assert!(set.get_mut(1).is_none());
        assert!(
            set.get(MAX_CHANNELS as u8).is_none(),
            "index == MAX_CHANNELS is out of range"
        );
        assert!(set.get(255).is_none());
    }

    #[test]
    fn set_ignores_an_out_of_range_index_rather_than_panicking() {
        let mut set = ChannelSet::new();
        let ch = ChannelConfig::default_primary();
        set.set(MAX_CHANNELS as u8, ch.clone());
        set.set(255, ch);
        // No panic, and no slot was created for either out-of-range index.
        assert!(set.get(MAX_CHANNELS as u8).is_none());
    }

    #[test]
    fn primary_finds_the_channel_with_the_primary_role_regardless_of_index() {
        let mut set = ChannelSet::new();
        // Move the primary role to a non-zero index.
        let mut primary = set.get(0).unwrap().clone();
        primary.index = 3;
        set.channels[0] = None;
        set.set(3, primary);

        let found = set.primary().expect("a primary channel exists at index 3");
        assert_eq!(found.index, 3);
    }

    #[test]
    fn active_channels_excludes_disabled_but_includes_secondary() {
        let mut set = ChannelSet::new();
        let mut secondary = ChannelConfig::default_primary();
        secondary.index = 1;
        secondary.role = ChannelRole::Secondary;
        set.set(1, secondary);

        let mut disabled = ChannelConfig::default_primary();
        disabled.index = 2;
        disabled.role = ChannelRole::Disabled;
        set.set(2, disabled);

        let active_indices: heapless::Vec<u8, 8> = set.active_channels().map(|c| c.index).collect();
        assert!(active_indices.contains(&0));
        assert!(active_indices.contains(&1));
        assert!(!active_indices.contains(&2));
    }

    #[test]
    fn is_encrypted_is_false_only_for_an_empty_psk() {
        let mut ch = ChannelConfig::default_primary();
        assert!(
            ch.is_encrypted(),
            "the default [0x01] sentinel counts as encrypted"
        );
        ch.psk.clear();
        assert!(!ch.is_encrypted());
    }
}
