use crate::{
    constants::MAX_HOP_LIMIT,
    domain::{
        context::MeshCtx,
        device::DeviceRole,
        radio_config::{ModemPreset, Region, bw_code_to_hz},
    },
    ports::MeshStorage,
    proto::{Config, config},
};
use log::{info, warn};

/// Valid custom spreading-factor range and fallback, matching upstream's
/// `LORA_SF_MIN`/`LORA_SF_MAX`/`LORA_SF_DEFAULT` (`src/mesh/MeshRadio.h`) —
/// an out-of-range value is clamped to the default, not rejected wholesale.
const CUSTOM_SF_RANGE: core::ops::RangeInclusive<u32> = 5..=12;
const CUSTOM_SF_DEFAULT: u8 = 11;
/// Valid custom coding-rate range and fallback, matching upstream's
/// `LORA_CR_MIN`/`LORA_CR_MAX`/`LORA_CR_DEFAULT`.
const CUSTOM_CR_RANGE: core::ops::RangeInclusive<u32> = 4..=8;
const CUSTOM_CR_DEFAULT: u8 = 5;
/// Fallback bandwidth when the phone sends an unmappable code (0), matching
/// upstream's `LORA_BW_DEFAULT_KHZ` = 250 kHz.
const CUSTOM_BW_DEFAULT_HZ: u32 = 250_000;

/// Clamp a custom spreading factor to `CUSTOM_SF_DEFAULT` if out of range.
/// Matches upstream's `clampSpreadFactor` (`src/mesh/MeshRadio.h`).
fn clamp_custom_sf(spread_factor: u32) -> u8 {
    if CUSTOM_SF_RANGE.contains(&spread_factor) {
        spread_factor as u8
    } else {
        warn!(
            "[Admin] Invalid spread_factor {}, using default {}",
            spread_factor, CUSTOM_SF_DEFAULT
        );
        CUSTOM_SF_DEFAULT
    }
}

/// Clamp a custom coding rate to `CUSTOM_CR_DEFAULT` if out of range.
/// Matches upstream's `clampCodingRate`.
fn clamp_custom_cr(coding_rate: u32) -> u8 {
    if CUSTOM_CR_RANGE.contains(&coding_rate) {
        coding_rate as u8
    } else {
        warn!(
            "[Admin] Invalid coding_rate {}, using default {}",
            coding_rate, CUSTOM_CR_DEFAULT
        );
        CUSTOM_CR_DEFAULT
    }
}

/// Resolve a `LoRaConfig.bandwidth` wire code to Hz, falling back to
/// `CUSTOM_BW_DEFAULT_HZ` for an unmappable (zero) code. Matches upstream's
/// `clampBandwidthKHz`, adapted for the fact that our `frequency_hz` divides
/// by this value as an integer and would panic on zero rather than upstream's
/// C++ float division, which silently produces `inf`/UB instead.
fn clamp_custom_bw_hz(bandwidth_code: u32) -> u32 {
    let hz = bw_code_to_hz(bandwidth_code as u16);
    if hz == 0 {
        warn!(
            "[Admin] Invalid bandwidth code {}, using default {} Hz",
            bandwidth_code, CUSTOM_BW_DEFAULT_HZ
        );
        CUSTOM_BW_DEFAULT_HZ
    } else {
        hz
    }
}

pub async fn handle<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, cfg: Config) {
    if let Some(variant) = cfg.payload_variant {
        match variant {
            config::PayloadVariant::Device(d) => {
                if let Ok(role) = DeviceRole::try_from(d.role) {
                    info!("[Admin] Setting device role to {:?}", role);
                    ctx.device.role = role;
                }
            }
            config::PayloadVariant::Lora(l) => {
                let region = Region::from_proto(l.region as u8);
                info!("[Admin] Setting region to {:?}", region);
                ctx.device.region = region as u8;

                // `use_preset` is stored exactly as sent, whether or not the
                // custom fields below are valid — matching upstream, which
                // never falls back to the preset path on a bad custom value.
                // Individual out-of-range fields are clamped to their own
                // default instead, same as upstream's clampSpreadFactor /
                // clampCodingRate. Bandwidth has no upstream equivalent
                // clamp at write time (upstream stores it unclamped, since
                // its C++ float division doesn't trap on zero) — but our
                // `frequency_hz` does divide by it as an integer at the next
                // boot, so an unmappable code(0) is clamped here rather than
                // left to panic later.
                ctx.device.use_preset = l.use_preset;

                let preset = ModemPreset::from_proto(l.modem_preset as u8);
                info!("[Admin] Setting modem preset to {:?}", preset);
                ctx.device.modem_preset = preset;

                let sf = clamp_custom_sf(l.spread_factor);
                let cr = clamp_custom_cr(l.coding_rate);
                let bw_hz = clamp_custom_bw_hz(l.bandwidth);
                info!(
                    "[Admin] Setting custom LoRa params: sf={} bw={} cr={}",
                    sf, bw_hz, cr
                );
                ctx.device.custom_sf = sf;
                ctx.device.custom_bw_hz = bw_hz;
                ctx.device.custom_cr = cr;

                ctx.device.channel_num = l.channel_num;

                let hop_limit = (l.hop_limit as u8).min(MAX_HOP_LIMIT);
                info!("[Admin] Setting hop limit to {}", hop_limit);
                ctx.device.hop_limit = hop_limit;

                info!("[Admin] Setting tx_enabled to {}", l.tx_enabled);
                ctx.device.tx_enabled = l.tx_enabled;
                ctx.tx_enabled
                    .store(l.tx_enabled, core::sync::atomic::Ordering::Relaxed);
            }
            _ => {
                info!("[Admin] SetConfig for other variants (ignored)");
            }
        }
        if let Err(e) = ctx.storage.save_state(ctx.device).await {
            warn!("[Admin] Failed to persist SetConfig change: {:?}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Per-field clamping — matches upstream's clampSpreadFactor /
    // clampCodingRate / clampBandwidthKHz: an out-of-range field falls back
    // to its own default, not to the preset path.
    // =========================================================================

    #[test]
    fn spread_factor_in_range_passes_through_unchanged() {
        for sf in [5u32, 7, 11, 12] {
            assert_eq!(clamp_custom_sf(sf), sf as u8);
        }
    }

    #[test]
    fn spread_factor_out_of_range_falls_back_to_the_default() {
        for sf in [0u32, 4, 13, 250] {
            assert_eq!(clamp_custom_sf(sf), CUSTOM_SF_DEFAULT);
        }
    }

    #[test]
    fn coding_rate_in_range_passes_through_unchanged() {
        for cr in [4u32, 5, 6, 8] {
            assert_eq!(clamp_custom_cr(cr), cr as u8);
        }
    }

    #[test]
    fn coding_rate_out_of_range_falls_back_to_the_default() {
        for cr in [0u32, 3, 9, 250] {
            assert_eq!(clamp_custom_cr(cr), CUSTOM_CR_DEFAULT);
        }
    }

    #[test]
    fn bandwidth_code_zero_falls_back_to_the_default() {
        // 0 is the one code bw_code_to_hz can't map to a nonzero value —
        // this is the case that would otherwise divide-by-zero in
        // frequency_hz at the next boot.
        assert_eq!(clamp_custom_bw_hz(0), CUSTOM_BW_DEFAULT_HZ);
    }

    #[test]
    fn bandwidth_code_valid_passes_through_converted_to_hz() {
        assert_eq!(clamp_custom_bw_hz(250), 250_000);
        assert_eq!(clamp_custom_bw_hz(31), 31_250); // fractional special case
    }
}
