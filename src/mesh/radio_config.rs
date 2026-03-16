//! Meshtastic radio modem presets and frequency configuration.
//!
//! Re-exports `RegionCode` (as `Region`) and `ModemPreset` from the proto-generated
//! code and adds radio-physics helper methods to each.

pub use crate::proto::config::lo_ra_config::{ModemPreset, RegionCode as Region};

/// djb2 hash (Dan Bernstein) — same algorithm as the official Meshtastic firmware.
/// Used for frequency slot computation when LoRaConfig.channel_num == 0.
pub const fn djb2(s: &[u8]) -> u32 {
    let mut h: u32 = 5381;
    let mut i = 0;
    while i < s.len() {
        h = h.wrapping_mul(33).wrapping_add(s[i] as u32);
        i += 1;
    }
    h
}

/// Radio parameters for a given modem preset
#[derive(Debug, Clone, Copy)]
pub struct ModemConfig {
    pub spreading_factor: u8,
    pub bandwidth_hz: u32,
    pub coding_rate: u8, // 5 = 4/5, 6 = 4/6, 7 = 4/7, 8 = 4/8
}

impl Region {
    /// Construct from protobuf LoRaConfig.RegionCode value.
    pub fn from_proto(v: u8) -> Self {
        Self::try_from(v as i32).unwrap_or(Self::Eu433)
    }

    /// Default channel index for this region when channel_num=0 (hash-based).
    ///
    /// Per proto spec, empty channel name is replaced by the preset display name.
    /// Hash uses djb2 (same as official firmware), then modulo num_channels.
    /// NOTE: When LoRaConfig.channel_num > 0, it is 1-indexed; subtract 1 first.
    pub const fn default_channel_index(self, preset: ModemPreset) -> u32 {
        let name = preset.display_name().as_bytes();
        let h = djb2(name);
        let bw = preset.config().bandwidth_hz;
        // Guard against zero bandwidth to avoid division by zero
        if bw == 0 {
            return 0;
        }
        let num_ch = self.band_hz() / bw;
        if num_ch == 0 { 0 } else { h % num_ch }
    }

    /// Protobuf enum value (matches LoRaConfig.RegionCode)
    pub const fn proto_value(self) -> u32 {
        self as u32
    }

    /// Band start frequency in Hz
    pub const fn start_hz(self) -> u32 {
        match self {
            Self::Us => 902_000_000,
            Self::Eu433 | Self::Ua433 | Self::My433 | Self::Ph433 | Self::Anz433 | Self::Kz433 => {
                433_000_000
            }
            Self::Eu868 | Self::Ua868 | Self::Ph868 | Self::Kz863 | Self::Np865 | Self::Nz865 => {
                869_400_000
            }
            Self::Anz | Self::Ph915 => 915_000_000,
            Self::Cn => 470_000_000,
            Self::Jp => 920_000_000,
            Self::Kr => 920_000_000,
            Self::Tw => 920_000_000,
            Self::Ru => 868_700_000,
            Self::In => 865_000_000,
            Self::Th => 920_000_000,
            Self::Lora24 => 2_400_000_000,
            Self::My919 => 919_000_000,
            Self::Sg923 => 923_000_000,
            Self::Br902 => 902_000_000,
            Self::Unset => 433_000_000,
        }
    }

    /// Band width in Hz
    pub const fn band_hz(self) -> u32 {
        match self {
            Self::Us | Self::Br902 => 26_000_000,
            Self::Eu433 | Self::Ua433 | Self::My433 | Self::Ph433 | Self::Anz433 | Self::Kz433 => {
                1_000_000
            }
            Self::Eu868 | Self::Ua868 | Self::Kz863 => 250_000,
            Self::Nz865 | Self::Np865 => 250_000,
            Self::Ph868 => 250_000,
            Self::Anz | Self::Ph915 => 13_000_000,
            Self::Cn => 26_000_000,
            Self::Jp => 4_000_000,
            Self::Kr => 2_000_000,
            Self::Tw => 2_000_000,
            Self::Ru => 250_000,
            Self::In => 1_000_000,
            Self::Th => 4_000_000,
            Self::Lora24 => 11_000_000,
            Self::My919 => 6_000_000,
            Self::Sg923 => 4_000_000,
            Self::Unset => 1_000_000,
        }
    }

    /// Number of channels for a given bandwidth
    pub const fn num_channels(self, bandwidth_hz: u32) -> u32 {
        self.band_hz() / bandwidth_hz
    }

    /// Frequency for a given channel index
    pub const fn frequency_hz(self, bandwidth_hz: u32, channel_index: u32) -> u32 {
        let num_ch = self.num_channels(bandwidth_hz);
        let ch = channel_index % num_ch;
        self.start_hz() + bandwidth_hz / 2 + ch * bandwidth_hz
    }
}

impl ModemPreset {
    /// Construct from protobuf LoRaConfig.ModemPreset value.
    pub fn from_proto(v: u8) -> Self {
        Self::try_from(v as i32).unwrap_or(Self::LongFast)
    }

    pub const fn config(self) -> ModemConfig {
        match self {
            Self::LongFast => ModemConfig {
                spreading_factor: 11,
                bandwidth_hz: 250_000,
                coding_rate: 5,
            },
            #[allow(deprecated)]
            Self::LongSlow => ModemConfig {
                spreading_factor: 12,
                bandwidth_hz: 125_000,
                coding_rate: 8,
            },
            #[allow(deprecated)]
            Self::VeryLongSlow => ModemConfig {
                spreading_factor: 12,
                bandwidth_hz: 62_500,
                coding_rate: 8,
            },
            Self::MediumSlow => ModemConfig {
                spreading_factor: 11,
                bandwidth_hz: 250_000,
                coding_rate: 8,
            },
            Self::MediumFast => ModemConfig {
                spreading_factor: 9,
                bandwidth_hz: 250_000,
                coding_rate: 5,
            },
            Self::ShortSlow => ModemConfig {
                spreading_factor: 8,
                bandwidth_hz: 250_000,
                coding_rate: 5,
            },
            Self::ShortFast => ModemConfig {
                spreading_factor: 7,
                bandwidth_hz: 250_000,
                coding_rate: 5,
            },
            Self::LongModerate => ModemConfig {
                spreading_factor: 11,
                bandwidth_hz: 125_000,
                coding_rate: 8,
            },
            Self::ShortTurbo => ModemConfig {
                spreading_factor: 7,
                bandwidth_hz: 500_000,
                coding_rate: 5,
            },
            Self::LongTurbo => ModemConfig {
                spreading_factor: 11,
                bandwidth_hz: 500_000,
                coding_rate: 5,
            },
        }
    }

    /// Display name used by the official firmware for channel hashing when channel name is empty.
    /// Matches `DisplayFormatters::getModemPresetDisplayName(preset, false, true)` in the official firmware.
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::LongFast => "LongFast",
            #[allow(deprecated)]
            Self::LongSlow => "LongSlow",
            #[allow(deprecated)]
            Self::VeryLongSlow => "VeryLongSlow",
            Self::MediumSlow => "MediumSlow",
            Self::MediumFast => "MediumFast",
            Self::ShortSlow => "ShortSlow",
            Self::ShortFast => "ShortFast",
            Self::LongModerate => "LongMod",
            Self::ShortTurbo => "ShortTurbo",
            Self::LongTurbo => "LongTurbo",
        }
    }

    /// Channel count for this preset in a given region
    pub const fn num_channels(self, region: Region) -> u32 {
        region.num_channels(self.config().bandwidth_hz)
    }

    /// Frequency for channel_index in a given region
    pub const fn frequency_hz(self, region: Region, channel_index: u32) -> u32 {
        region.frequency_hz(self.config().bandwidth_hz, channel_index)
    }
}
