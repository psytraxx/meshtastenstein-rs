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

/// Convert a `LoRaConfig.bandwidth` wire code to Hz.
///
/// The proto field is a *code*, not raw kHz: most values are the kHz figure
/// directly, but six carry fractional kHz values the code can't represent
/// (e.g. code `800` means 812.5 kHz, not 800 kHz). Matches upstream's
/// `bwCodeToKHz` (`src/mesh/MeshRadio.h`). `0` maps to `0` — callers must
/// treat that as "no valid bandwidth", not silently default it here.
pub const fn bw_code_to_hz(code: u16) -> u32 {
    match code {
        31 => 31_250,
        62 => 62_500,
        200 => 203_125,
        400 => 406_250,
        800 => 812_500,
        1600 => 1_625_000,
        other => (other as u32) * 1000,
    }
}

/// Convert a bandwidth in Hz back to the `LoRaConfig.bandwidth` wire code.
/// Inverse of `bw_code_to_hz`; matches upstream's `bwKHzToCode`.
pub const fn bw_hz_to_code(bandwidth_hz: u32) -> u16 {
    match bandwidth_hz {
        31_250 => 31,
        62_500 => 62,
        203_125 => 200,
        406_250 => 400,
        812_500 => 800,
        1_625_000 => 1600,
        other => (other / 1000) as u16,
    }
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
            #[allow(deprecated)]
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
            // ITU Region 2 amateur 1.25 m ('125cm') band: 220–225 MHz
            Self::Itu2125cm => 220_000_000,
            // ITU Region 1/3 amateur 70cm band: 430–440/450 MHz
            Self::Itu170cm | Self::Itu370cm => 430_000_000,
            // ITU Region 2 amateur 70cm band: 420–450 MHz
            Self::Itu270cm => 420_000_000,
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
            #[allow(deprecated)]
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
            Self::Itu2125cm => 225_000_000,
            Self::Itu170cm => 440_000_000,
            Self::Itu270cm | Self::Itu370cm => 450_000_000,
        }
    }

    /// Band width in Hz (`freq_end_hz - freq_start_hz`).
    pub const fn band_hz(self) -> u32 {
        self.freq_end_hz() - self.freq_start_hz()
    }

    /// Number of channels for a given bandwidth. Returns 0 for a zero
    /// bandwidth rather than dividing by it — see `frequency_hz`.
    pub const fn num_channels(self, bandwidth_hz: u32) -> u32 {
        if bandwidth_hz == 0 {
            return 0;
        }
        self.band_hz() / bandwidth_hz
    }

    /// Frequency for a given channel index.
    ///
    /// Returns `freq_start_hz()` (channel 0, no offset applied) for a zero
    /// bandwidth instead of panicking. This can only be reached with a
    /// malformed custom LoRa config — validated against at the admin
    /// boundary (`set_config::handle`) — but the guard stays here too since
    /// this is a `const fn` a future caller could reach without going
    /// through that validation.
    ///
    /// Deliberately still the pre-2.8 flat model (`freq_start + bw/2 + ch*bw`
    /// over `band_hz() / bw` channels), not upstream 2.8.0's per-region
    /// `RegionProfile` rework (channel `spacing`/`padding` plus an
    /// `overrideSlot` of explicit-slot / preset-hash / channel-hash). The two
    /// models are algebraically identical for every region using
    /// PROFILE_STD/PROFILE_EU868/PROFILE_UNDEF (spacing=0, padding=0) — which
    /// covers every region either board realistically runs, so EU_433/
    /// LongFast is still 433.875 MHz. They diverge for `Eu866` (LITE),
    /// `EuN868` (NARROW), and every ITU amateur-band region (HAM_20KHZ/
    /// HAM_100KHZ): this function will compute a different frequency than a
    /// stock 2.8 node for those. See the README's Known Limitations.
    pub const fn frequency_hz(self, bandwidth_hz: u32, channel_index: u32) -> u32 {
        let num_ch = self.num_channels(bandwidth_hz);
        if num_ch == 0 {
            return self.freq_start_hz();
        }
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
            // Amateur radio bands: upstream's `regions[]` table gives these
            // no regulatory duty-cycle restriction (duty_cycle=100).
            | Self::Itu12m
            | Self::Itu22m
            | Self::Itu32m
            | Self::Itu2125cm
            | Self::Itu170cm
            | Self::Itu270cm
            | Self::Itu370cm => 100.0,
            Self::Eu433 | Self::Th | Self::Ua433 => 10.0,
            #[allow(deprecated)]
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
            Self::ShortFast | Self::ShortTurbo | Self::NarrowFast | Self::TinyFast => 7,
            Self::ShortSlow | Self::NarrowSlow | Self::TinySlow => 8,
            Self::MediumFast | Self::LiteFast | Self::MediumTurbo => 9,
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
            Self::ShortTurbo | Self::LongTurbo | Self::MediumTurbo => 500_000,
            // True SX1262 bandwidth is 15.63 kHz; 15_600 has no exact
            // `LoRaConfig.bandwidth` wire code (upstream's own `bwKHzToCode`
            // rounds 15.6 to code 16, which `bw_code_to_hz` maps to 16_000 —
            // a lossy round-trip that only matters on the custom-config wire
            // path, never when a preset is selected). Do not add a `16 =>
            // 15_600` special case to `bw_code_to_hz`/`bw_hz_to_code` to
            // "fix" this — that would silently redefine an existing code.
            Self::TinyFast | Self::TinySlow => 15_600,
            _ => 250_000, // LongFast, MediumSlow, MediumFast, ShortSlow, ShortFast
        }
    }

    pub const fn coding_rate(self) -> u8 {
        match self {
            #[allow(deprecated)]
            Self::LongSlow | Self::VeryLongSlow | Self::MediumSlow | Self::LongModerate => 8,
            Self::NarrowFast | Self::NarrowSlow | Self::TinySlow => 6,
            Self::TinyFast | Self::MediumTurbo => 5,
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
            Self::TinyFast => "TinyFast",
            Self::TinySlow => "TinySlow",
            Self::MediumTurbo => "MediumTurbo",
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

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Bandwidth code <-> Hz conversion — six fractional special cases plus
    // the plain-integer fallback, matching upstream's bwCodeToKHz/bwKHzToCode.
    // =========================================================================

    #[test]
    fn bandwidth_code_round_trips_the_six_fractional_cases() {
        for (code, hz) in [
            (31u16, 31_250u32),
            (62, 62_500),
            (200, 203_125),
            (400, 406_250),
            (800, 812_500),
            (1600, 1_625_000),
        ] {
            assert_eq!(bw_code_to_hz(code), hz);
            assert_eq!(bw_hz_to_code(hz), code);
        }
    }

    #[test]
    fn bandwidth_code_round_trips_a_plain_integer_code() {
        // 250 (kHz) is not one of the six fractional special cases — it maps
        // straight through as a plain integer, matching every default
        // preset's bandwidth in this firmware.
        assert_eq!(bw_code_to_hz(250), 250_000);
        assert_eq!(bw_hz_to_code(250_000), 250);
    }

    // =========================================================================
    // Zero-bandwidth guard — a malformed custom config (validated against at
    // the admin boundary, but this is a `const fn` a future caller could
    // reach directly) must not panic on divide-by-zero.
    // =========================================================================

    #[test]
    fn frequency_hz_with_zero_bandwidth_returns_band_start_instead_of_panicking() {
        let region = Region::Eu433;
        assert_eq!(region.frequency_hz(0, 5), region.freq_start_hz());
    }

    #[test]
    fn num_channels_with_zero_bandwidth_returns_zero() {
        assert_eq!(Region::Eu433.num_channels(0), 0);
    }

    // =========================================================================
    // v2.8.0 additions: three new modem presets, four new ITU amateur-band
    // regions. Display names feed the channel hash directly, so an exact
    // literal match matters for interop; SF/BW/CR guard against the
    // catch-all-arm regression a naive match extension would reintroduce.
    // =========================================================================

    #[test]
    fn new_preset_display_names_match_upstream_exactly() {
        assert_eq!(ModemPreset::TinyFast.display_name(), "TinyFast");
        assert_eq!(ModemPreset::TinySlow.display_name(), "TinySlow");
        assert_eq!(ModemPreset::MediumTurbo.display_name(), "MediumTurbo");
    }

    #[test]
    fn new_preset_radio_params_match_upstream() {
        let tiny_fast = ModemPreset::TinyFast.config();
        assert_eq!(tiny_fast.spreading_factor, 7);
        assert_eq!(tiny_fast.bandwidth_hz, 15_600);
        assert_eq!(tiny_fast.coding_rate, 5);

        let tiny_slow = ModemPreset::TinySlow.config();
        assert_eq!(tiny_slow.spreading_factor, 8);
        assert_eq!(tiny_slow.bandwidth_hz, 15_600);
        assert_eq!(tiny_slow.coding_rate, 6);

        let medium_turbo = ModemPreset::MediumTurbo.config();
        assert_eq!(medium_turbo.spreading_factor, 9);
        assert_eq!(medium_turbo.bandwidth_hz, 500_000);
        assert_eq!(medium_turbo.coding_rate, 5);
    }

    #[test]
    fn new_ham_regions_have_correct_band_edges() {
        assert_eq!(Region::Itu2125cm.freq_start_hz(), 220_000_000);
        assert_eq!(Region::Itu2125cm.freq_end_hz(), 225_000_000);
        assert_eq!(Region::Itu2125cm.band_hz(), 5_000_000);

        assert_eq!(Region::Itu170cm.freq_start_hz(), 430_000_000);
        assert_eq!(Region::Itu170cm.freq_end_hz(), 440_000_000);
        assert_eq!(Region::Itu170cm.band_hz(), 10_000_000);

        assert_eq!(Region::Itu270cm.freq_start_hz(), 420_000_000);
        assert_eq!(Region::Itu270cm.freq_end_hz(), 450_000_000);
        assert_eq!(Region::Itu270cm.band_hz(), 30_000_000);

        assert_eq!(Region::Itu370cm.freq_start_hz(), 430_000_000);
        assert_eq!(Region::Itu370cm.freq_end_hz(), 450_000_000);
        assert_eq!(Region::Itu370cm.band_hz(), 20_000_000);
    }

    #[test]
    fn eu433_longfast_frequency_is_unchanged_by_the_v2_8_bump() {
        // Regression guard for the Phase 6 deferral: PROFILE_STD regions
        // (spacing=0, padding=0) are algebraically identical between the
        // pre-2.8 flat model this crate still uses and upstream's 2.8.0
        // RegionProfile rework, so this must still be 433.875 MHz.
        let region = Region::Eu433;
        let preset = ModemPreset::LongFast;
        let ch = region.default_channel_index(preset);
        assert_eq!(preset.frequency_hz(region, ch), 433_875_000);
    }
}
