//! Meshtastic radio modem presets and frequency configuration

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

    /// Get the number of frequency slots for this preset in US region
    /// Formula: (928 MHz - 902 MHz) / bandwidth
    pub const fn us_num_channels(self) -> u32 {
        let cfg = self.config();
        26_000_000 / cfg.bandwidth_hz
    }

    /// Calculate the frequency for a given channel index in US region
    /// Formula: 902 MHz + bandwidth/2 + channel_index * bandwidth
    pub const fn us_frequency_hz(self, channel_index: u32) -> u32 {
        let num_ch = self.us_num_channels();
        let ch = channel_index % num_ch;
        let cfg = self.config();
        902_000_000 + cfg.bandwidth_hz / 2 + ch * cfg.bandwidth_hz
    }
}
