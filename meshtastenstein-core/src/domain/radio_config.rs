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

    /// Band start frequency in Hz, `freq_start` from the upstream firmware's
    /// `regions[]` table (`src/mesh/RadioInterface.cpp`,
    /// `RDEF(name, freq_start, freq_end, duty_cycle, ...)`).
    ///
    /// One arm per region, in the same order as the C++ table, so this can be
    /// diffed against it directly. `Unset` intentionally mirrors `Us` ("Same as
    /// US" per the upstream comment).
    pub const fn freq_start_hz(self) -> u32 {
        match self {
            Self::Us | Self::Br902 | Self::Unset => 902_000_000,
            Self::Eu433 | Self::Ua433 | Self::My433 | Self::Ph433 => 433_000_000,
            Self::Eu868 | Self::EuN868 => 869_400_000,
            Self::Cn => 470_000_000,
            Self::Jp => 920_500_000,
            Self::Anz => 915_000_000,
            Self::Anz433 => 433_050_000,
            Self::Ru => 868_700_000,
            Self::Kr | Self::Tw | Self::Th => 920_000_000,
            Self::In | Self::Np865 => 865_000_000,
            Self::Nz865 => 864_000_000,
            Self::Ua868 | Self::Ph868 => 868_000_000,
            Self::My919 => 919_000_000,
            Self::Sg923 | Self::Eu917 => 917_000_000,
            Self::Ph915 => 915_000_000,
            Self::Kz433 => 433_075_000,
            Self::Kz863 => 863_000_000,
            Self::Lora24 => 2_400_000_000,
            // Not present in the referenced upstream `regions[]` snapshot.
            // EU 866 MHz SRD band (Band 47b of 2006/771/EC): 865.6–867.6 MHz
            Self::Eu866 => 865_600_000,
            // EU 874 MHz SRD band (Band 1 of 2022/172/EC): 874.0–874.4 MHz
            Self::Eu874 => 874_000_000,
            // ITU Region 1/2/3 amateur 2 m bands: 144–146/148 MHz
            Self::Itu12m | Self::Itu22m | Self::Itu32m => 144_000_000,
        }
    }

    /// Band end frequency in Hz, `freq_end` from the upstream firmware's `regions[]` table.
    pub const fn freq_end_hz(self) -> u32 {
        match self {
            Self::Us | Self::Anz | Self::Unset => 928_000_000,
            Self::Eu433 => 434_000_000,
            Self::Eu868 | Self::EuN868 => 869_650_000,
            Self::Cn => 510_000_000,
            Self::Jp => 923_500_000,
            Self::Anz433 => 434_790_000,
            Self::Ru => 869_200_000,
            Self::Kr => 923_000_000,
            Self::Tw | Self::Th => 925_000_000,
            Self::In => 867_000_000,
            Self::Nz865 | Self::Kz863 | Self::Np865 => 868_000_000,
            Self::Ua433 | Self::Ph433 => 434_700_000,
            Self::Ua868 => 868_600_000,
            Self::My433 => 435_000_000,
            Self::My919 => 924_000_000,
            Self::Sg923 => 925_000_000,
            Self::Ph868 => 869_400_000,
            Self::Ph915 | Self::Eu917 => 918_000_000,
            Self::Kz433 => 434_775_000,
            Self::Br902 => 907_500_000,
            Self::Lora24 => 2_483_500_000,
            // Not present in the referenced upstream `regions[]` snapshot.
            Self::Eu866 => 867_600_000,
            Self::Eu874 => 874_400_000,
            Self::Itu12m => 146_000_000,
            Self::Itu22m | Self::Itu32m => 148_000_000,
        }
    }

    /// Band width in Hz (`freq_end_hz - freq_start_hz`).
    pub const fn band_hz(self) -> u32 {
        self.freq_end_hz() - self.freq_start_hz()
    }

    /// Number of channels for a given bandwidth
    pub const fn num_channels(self, bandwidth_hz: u32) -> u32 {
        self.band_hz() / bandwidth_hz
    }

    /// Frequency for a given channel index
    pub const fn frequency_hz(self, bandwidth_hz: u32, channel_index: u32) -> u32 {
        let num_ch = self.num_channels(bandwidth_hz);
        let ch = channel_index % num_ch;
        self.freq_start_hz() + bandwidth_hz / 2 + ch * bandwidth_hz
    }

    /// Regulatory TX duty-cycle ceiling for this region, as a percentage of time.
    ///
    /// Matches the `dutyCycle` field of `RegionInfo` in the upstream firmware
    /// (`src/mesh/RadioInterface.cpp`). A value of `100.0` means "unlimited"
    /// (regions that enforce only airtime limits rather than a hard duty cycle).
    /// Our airtime gate treats traffic as "polite" when the rolling-window TX
    /// airtime is under half this ceiling.
    /// `duty_cycle` from the upstream firmware's `regions[]` table, in the same
    /// order as the C++ table so this can be diffed against it directly.
    pub const fn duty_cycle_pct(self) -> f32 {
        match self {
            Self::Us
            | Self::Cn
            | Self::Jp
            | Self::Anz
            | Self::Anz433
            | Self::Ru
            | Self::Kr
            | Self::Tw
            | Self::In
            | Self::Nz865
            | Self::My433
            | Self::My919
            | Self::Sg923
            | Self::Ph433
            | Self::Ph868
            | Self::Ph915
            | Self::Kz433
            | Self::Kz863
            | Self::Np865
            | Self::Br902
            | Self::Lora24
            // "Same as US" per the upstream comment.
            | Self::Unset
            // Not present in the referenced upstream `regions[]` snapshot;
            // amateur radio bands have no regulatory duty-cycle restriction.
            | Self::Itu12m
            | Self::Itu22m
            | Self::Itu32m => 100.0,
            Self::Eu433 | Self::Th | Self::Ua433 => 10.0,
            Self::Eu868 | Self::Ua868 => 1.0,
            // Not present in the referenced upstream `regions[]` snapshot.
            // EU 866 SRD: 2.5% (Band 47b of 2006/771/EC)
            Self::Eu866 => 2.5,
            // EU narrow 868 SRD: 10% (Band 47 of 2006/771/EC)
            Self::EuN868 => 10.0,
            // EU 874/917 SRD (2022/172/EC): conservative 1% until firmware specifies
            Self::Eu874 | Self::Eu917 => 1.0,
        }
    }
}

impl ModemPreset {
    /// Construct from protobuf LoRaConfig.ModemPreset value.
    pub fn from_proto(v: u8) -> Self {
        Self::try_from(v as i32).unwrap_or(Self::LongFast)
    }

    pub const fn spreading_factor(self) -> u8 {
        match self {
            Self::ShortFast | Self::ShortTurbo | Self::NarrowFast => 7,
            Self::ShortSlow | Self::NarrowSlow => 8,
            Self::MediumFast | Self::LiteFast => 9,
            Self::LiteSlow => 10,
            #[allow(deprecated)]
            Self::LongSlow | Self::VeryLongSlow => 12,
            _ => 11, // LongFast, MediumSlow, LongModerate, LongTurbo
        }
    }

    pub const fn bandwidth_hz(self) -> u32 {
        match self {
            #[allow(deprecated)]
            Self::VeryLongSlow | Self::NarrowFast | Self::NarrowSlow => 62_500,
            #[allow(deprecated)]
            Self::LongSlow | Self::LongModerate | Self::LiteFast | Self::LiteSlow => 125_000,
            Self::ShortTurbo | Self::LongTurbo => 500_000,
            _ => 250_000, // LongFast, MediumSlow, MediumFast, ShortSlow, ShortFast
        }
    }

    pub const fn coding_rate(self) -> u8 {
        match self {
            #[allow(deprecated)]
            Self::LongSlow | Self::VeryLongSlow | Self::MediumSlow | Self::LongModerate => 8,
            Self::NarrowFast | Self::NarrowSlow => 6,
            _ => 5,
        }
    }

    pub const fn config(self) -> ModemConfig {
        ModemConfig {
            spreading_factor: self.spreading_factor(),
            bandwidth_hz: self.bandwidth_hz(),
            coding_rate: self.coding_rate(),
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
            Self::LiteFast => "LiteFast",
            Self::LiteSlow => "LiteSlow",
            Self::NarrowFast => "NarrowFast",
            Self::NarrowSlow => "NarrowSlow",
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
