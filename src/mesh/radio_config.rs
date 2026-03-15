//! Meshtastic radio modem presets and frequency configuration

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

/// Meshtastic region codes (matches config.proto LoRaConfig.RegionCode)
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[repr(u8)]
pub enum Region {
    /// United States: 902–928 MHz, 26 MHz band
    US = 1,
    /// EU 433 MHz: 433–434 MHz, 1 MHz band
    #[default]
    EU433 = 2,
    /// EU 868 MHz: 869.4–869.65 MHz, 0.25 MHz band
    EU868 = 3,
    /// Australia/NZ: 915–928 MHz
    ANZ = 6,
}

impl Region {
    /// Construct from protobuf LoRaConfig.RegionCode value
    pub const fn from_proto(v: u8) -> Self {
        match v {
            1 => Self::US,
            2 => Self::EU433,
            3 => Self::EU868,
            6 => Self::ANZ,
            _ => Self::EU433,
        }
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
            Self::US => 902_000_000,
            Self::EU433 => 433_000_000,
            Self::EU868 => 869_400_000,
            Self::ANZ => 915_000_000,
        }
    }

    /// Band width in Hz
    pub const fn band_hz(self) -> u32 {
        match self {
            Self::US => 26_000_000,
            Self::EU433 => 1_000_000,
            Self::EU868 => 250_000,
            Self::ANZ => 13_000_000,
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

/// Modem presets matching Meshtastic's config.proto ModemPreset enum
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[repr(u8)]
pub enum ModemPreset {
    #[default]
    LongFast = 0,
    LongSlow = 1,
    VeryLongSlow = 2,
    MediumSlow = 3,
    MediumFast = 4,
    ShortSlow = 5,
    ShortFast = 6,
    LongModerate = 7,
}

/// Radio parameters for a given modem preset
#[derive(Debug, Clone, Copy)]
pub struct ModemConfig {
    pub spreading_factor: u8,
    pub bandwidth_hz: u32,
    pub coding_rate: u8, // 5 = 4/5, 6 = 4/6, 7 = 4/7, 8 = 4/8
}

impl ModemPreset {
    /// Construct from protobuf LoRaConfig.ModemPreset value
    pub const fn from_proto(v: u8) -> Self {
        match v {
            0 => Self::LongFast,
            1 => Self::LongSlow,
            2 => Self::VeryLongSlow,
            3 => Self::MediumSlow,
            4 => Self::MediumFast,
            5 => Self::ShortSlow,
            6 => Self::ShortFast,
            7 => Self::LongModerate,
            _ => Self::LongFast,
        }
    }

    pub const fn config(self) -> ModemConfig {
        match self {
            Self::LongFast => ModemConfig {
                spreading_factor: 11,
                bandwidth_hz: 250_000,
                coding_rate: 5,
            },
            Self::LongSlow => ModemConfig {
                spreading_factor: 12,
                bandwidth_hz: 125_000,
                coding_rate: 8,
            },
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
        }
    }

    /// Display name used by the official firmware for channel hashing when channel name is empty.
    /// Matches `DisplayFormatters::getModemPresetDisplayName(preset, false, true)` in the official firmware.
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::LongFast => "LongFast",
            Self::LongSlow => "LongSlow",
            Self::VeryLongSlow => "VeryLongSlow",
            Self::MediumSlow => "MediumSlow",
            Self::MediumFast => "MediumFast",
            Self::ShortSlow => "ShortSlow",
            Self::ShortFast => "ShortFast",
            Self::LongModerate => "LongMod",
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
