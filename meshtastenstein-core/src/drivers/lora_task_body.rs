//! Board-agnostic LoRa task logic, shared by every board's `lora_task`.
//!
//! Everything here operates purely on `lora_phy::LoRa<RK, DLY>` (generic over
//! its `RadioKind`/`DelayNs` trait bounds, same as `sx1262_direct.rs` is
//! generic over embedded-hal traits) plus the mesh Embassy channels — no SPI,
//! GPIO, or board type ever appears. Each board's own `lora_task` is
//! responsible only for: constructing the SPI bus and GPIO pins, initializing
//! `lora_phy::LoRa`, writing the Meshtastic sync word, and then handing the
//! initialized radio to [`run`] for the rest of its life.

use crate::{
    constants::*,
    domain::{packet::RadioFrame, radio_config::ModemConfig},
    inter_task::channels::{MeshEvent, RadioMetadata},
};
use embassy_futures::select::{Either3, select3};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    channel::{Receiver, Sender},
};
use embassy_time::{Duration, Instant, Timer};
use lora_phy::{
    DelayNs, LoRa, RxMode,
    mod_params::{Bandwidth, CodingRate, DutyCycleParams, SpreadingFactor},
    mod_traits::RadioKind,
    sx126x::{Config as Sx126xConfig, Sx1262, TcxoCtrlVoltage},
};

extern crate alloc;
use alloc::boxed::Box;

/// Board-independent SX1262 chip config, shared by every board's
/// `lora_task`. A single definition here makes the TCXO/DCDC/`rx_boost`
/// choices one reviewable decision instead of two copies that can silently
/// drift — worth doing since `rx_boost: true` is itself a deliberate
/// deviation from upstream Meshtastic's default (RadioLib leaves it
/// configurable; this firmware hardcodes it on).
pub fn meshtastic_sx1262_config() -> Sx126xConfig<Sx1262> {
    Sx126xConfig {
        chip: Sx1262,
        tcxo_ctrl: Some(TcxoCtrlVoltage::Ctrl1V8),
        use_dcdc: true,
        rx_boost: true,
    }
}

/// Map Meshtastic's wire bandwidth (Hz) to lora-phy's enum. Unrecognized
/// values fall back to 250 kHz, matching upstream's LongFast default.
fn map_bandwidth(bandwidth_hz: u32) -> Bandwidth {
    match bandwidth_hz {
        7_800 => Bandwidth::_7KHz,
        10_400 => Bandwidth::_10KHz,
        15_600 => Bandwidth::_15KHz,
        20_800 => Bandwidth::_20KHz,
        31_250 => Bandwidth::_31KHz,
        41_700 => Bandwidth::_41KHz,
        62_500 => Bandwidth::_62KHz,
        125_000 => Bandwidth::_125KHz,
        250_000 => Bandwidth::_250KHz,
        500_000 => Bandwidth::_500KHz,
        _ => Bandwidth::_250KHz,
    }
}

/// Map Meshtastic's wire spreading factor to lora-phy's enum. Unrecognized
/// values fall back to SF11, matching upstream's LongFast default.
fn map_spreading_factor(spreading_factor: u8) -> SpreadingFactor {
    match spreading_factor {
        5 => SpreadingFactor::_5,
        6 => SpreadingFactor::_6,
        7 => SpreadingFactor::_7,
        8 => SpreadingFactor::_8,
        9 => SpreadingFactor::_9,
        10 => SpreadingFactor::_10,
        11 => SpreadingFactor::_11,
        12 => SpreadingFactor::_12,
        _ => SpreadingFactor::_11,
    }
}

/// Map Meshtastic's wire coding rate to lora-phy's enum. Unrecognized values
/// fall back to 4/5, matching upstream's LongFast default.
fn map_coding_rate(coding_rate: u8) -> CodingRate {
    match coding_rate {
        5 => CodingRate::_4_5,
        6 => CodingRate::_4_6,
        7 => CodingRate::_4_7,
        8 => CodingRate::_4_8,
        _ => CodingRate::_4_5,
    }
}

/// Minimum number of symbols the radio must stay awake for after detecting a
/// preamble, before it's confident enough to keep listening for the header.
/// Matches upstream's `startReceiveDutyCycleAuto(preambleLength, 8, ...)` —
/// RadioLib's own default for SF>6 would also be 8, so this isn't a magic
/// number, it's what upstream passes explicitly.
const RX_DUTY_CYCLE_MIN_SYMBOLS: u32 = 8;

/// SX1262 timer tick used by `SetRxDutyCycle`'s raw 24-bit fields.
const SX1262_TIMER_TICK_US: u32 = 15625; // 15.625 us, i.e. 1/64000 s, x1000 for integer math
const SX1262_TIMER_TICK_US_DIV: u32 = 1000;

/// Compute SX1262 hardware RX duty-cycle timings, porting RadioLib's
/// `PhysicalLayer::calculateRxDutyCycle` (the function behind upstream's
/// `startReceiveDutyCycleAuto`, used by every SX126x Meshtastic board).
///
/// `sender_preamble_symbols` is deliberately a separate parameter from our
/// own RX preamble length — the algorithm is about how long a *transmitting*
/// node's preamble is, not how long we listen for. Passing our own
/// `MESHTASTIC_PREAMBLE_LENGTH` here would be a mistake even though today
/// they happen to hold the same value.
///
/// Returns `None` when duty cycling can't produce a meaningful sleep window
/// (either the preamble is too short relative to `RX_DUTY_CYCLE_MIN_SYMBOLS`,
/// matching RadioLib's `2 * minSymbols > senderPreambleLength` guard, or the
/// resulting sleep period is too short to be worth the mode-transition
/// overhead, matching RadioLib's own `sleepPeriod < tcxoDelay + 1016us`
/// fallback) — callers should fall back to `RxMode::Continuous` in that case.
/// Unlike RadioLib, lora-phy does neither check itself; it programs exactly
/// what it's given, so both checks live here.
fn rx_duty_cycle_params(
    sender_preamble_symbols: u16,
    spreading_factor: u8,
    bandwidth_hz: u32,
) -> Option<DutyCycleParams> {
    let sender_preamble_symbols = sender_preamble_symbols as u32;
    let min_symbols = RX_DUTY_CYCLE_MIN_SYMBOLS;

    if 2 * min_symbols > sender_preamble_symbols {
        return None;
    }
    let sleep_symbols = sender_preamble_symbols - 2 * min_symbols;

    // symbol_length (us) = (10_000 << sf) / (10 * bandwidth_khz), integer math
    // matching RadioLib exactly (it also computes in microseconds).
    let bandwidth_khz = bandwidth_hz / 1000;
    if bandwidth_khz == 0 {
        return None;
    }
    let symbol_length_us = (10_000u32 << spreading_factor.min(31)) / (10 * bandwidth_khz);

    let sleep_period_us = symbol_length_us * sleep_symbols;

    // RadioLib's own fallback: not worth the mode transition below this.
    // tcxoDelay varies by board; use a fixed conservative 1016us margin
    // (RadioLib's constant term) without a board-specific TCXO delay term,
    // since lora-phy's `set_tcxo_ctrl` already waits it out during init, not
    // per-transition — matching lora-phy's actual per-transition cost.
    const MIN_WORTHWHILE_SLEEP_US: u32 = 1016;
    if sleep_period_us < MIN_WORTHWHILE_SLEEP_US {
        return None;
    }

    // wakePeriod = max( (symLen*(preamble+1) - (sleepPeriod-1000)) / 2,
    //                   symLen*(minSymbols+1) )
    let a = (symbol_length_us * (sender_preamble_symbols + 1))
        .saturating_sub(sleep_period_us.saturating_sub(1000))
        / 2;
    let b = symbol_length_us * (min_symbols + 1);
    let wake_period_us = a.max(b);

    Some(DutyCycleParams {
        rx_time: us_to_sx1262_ticks(wake_period_us),
        sleep_time: us_to_sx1262_ticks(sleep_period_us),
    })
}

/// Convert a microsecond duration to the SX1262's 15.625us timer ticks used
/// by `SetRxDutyCycle`'s raw fields. lora-phy packs these straight into the
/// 24-bit register payload with no scaling of its own
/// (`sx126x/mod.rs`'s `do_rx`), so getting this conversion wrong silently
/// produces periods off by the same factor as the tick size (64x).
fn us_to_sx1262_ticks(us: u32) -> u32 {
    (us * SX1262_TIMER_TICK_US_DIV) / SX1262_TIMER_TICK_US
}

/// Build modulation params for `modem_cfg`/`frequency_hz`, falling back to
/// Meshtastic's own LongFast default (SF11/250kHz/CR4-5) if the combination
/// is rejected — e.g. lora-phy refuses a 250/500 kHz bandwidth below 400 MHz,
/// which the ITU 144-148 MHz amateur regions fall under. A user-selectable
/// region+preset combination can hit this from a saved config, so this must
/// not panic the radio task.
async fn modulation_params_with_fallback<RK: RadioKind, DLY: DelayNs>(
    lora: &mut LoRa<RK, DLY>,
    modem_cfg: &ModemConfig,
    frequency_hz: u32,
) -> lora_phy::mod_params::ModulationParams {
    let bandwidth = map_bandwidth(modem_cfg.bandwidth_hz);
    let sf = map_spreading_factor(modem_cfg.spreading_factor);
    let cr = map_coding_rate(modem_cfg.coding_rate);

    lora.create_modulation_params(sf, bandwidth, cr, frequency_hz)
        .unwrap_or_else(|e| {
            log::error!(
                "[LoRa] Invalid modulation params (SF={:?} BW={:?} CR={:?} @ {}Hz): {:?} — falling back to LongFast",
                sf, bandwidth, cr, frequency_hz, e
            );
            lora.create_modulation_params(
                SpreadingFactor::_11,
                Bandwidth::_250KHz,
                CodingRate::_4_5,
                frequency_hz,
            )
            .expect("LongFast fallback modulation params must be valid")
        })
}

/// Compute and send a `ChannelUtilUpdate` event. Broken out of `run`'s main
/// loop only to avoid repeating the same six-line block at each of its three
/// call sites (silent-channel timeout, post-TX, post-RX) — see the doc
/// comment above `HEARTBEAT_INTERVAL`/`SILENT_CHANNEL_REPORT_INTERVAL` inside
/// `run` for why there are three sites instead of one shared timer tick.
fn report_channel_util(
    mesh_in: &Sender<'static, CriticalSectionRawMutex, MeshEvent, 8>,
    tx_airtime_ms: u64,
    rx_airtime_ms: u64,
    util_window_start: Instant,
    rx_count: u32,
    tx_count: u32,
) {
    let elapsed_ms = util_window_start.elapsed().as_millis().max(1) as f32;
    let total_airtime_ms = (tx_airtime_ms + rx_airtime_ms) as f32;
    let channel_util_pct = (total_airtime_ms / elapsed_ms) * 100.0;
    let air_util_tx_pct = (tx_airtime_ms as f32 / elapsed_ms) * 100.0;
    let _ = mesh_in.try_send(MeshEvent::ChannelUtilUpdate(
        channel_util_pct,
        air_util_tx_pct,
    ));
    log::info!(
        "[LoRa] RX loop alive: rx={} tx={} util={:.1}%",
        rx_count,
        tx_count,
        channel_util_pct
    );
}

/// Bring an already-initialized `LoRa` radio into RX (hardware duty-cycled
/// when the modem params support it, continuous otherwise — see
/// `rx_duty_cycle_params`) and run the TX/RX/CAD/channel-utilization event
/// loop for the lifetime of the task. Never returns.
///
/// The caller is responsible for everything before this: SPI/GPIO setup,
/// `LoRa::new()`, and writing the Meshtastic sync word (which must happen
/// after `LoRa::new()`'s internal chip reset, or it gets wiped).
pub async fn run<RK: RadioKind, DLY: DelayNs>(
    lora: &mut LoRa<RK, DLY>,
    modem_cfg: &ModemConfig,
    frequency_hz: u32,
    node_num: u32,
    tx_queue: Receiver<'static, CriticalSectionRawMutex, RadioFrame, 5>,
    mesh_in: Sender<'static, CriticalSectionRawMutex, MeshEvent, 8>,
) -> ! {
    log::info!("[LoRa] Radio initialized, configuring modulation...");

    let modulation_params = modulation_params_with_fallback(lora, modem_cfg, frequency_hz).await;

    let mut tx_packet_params = lora
        .create_tx_packet_params(
            MESHTASTIC_PREAMBLE_LENGTH,
            false, // implicit header = false
            true,  // CRC on
            false, // IQ inversion off
            &modulation_params,
        )
        .expect("tx packet params derive only from already-validated modulation params");

    let rx_packet_params = lora
        .create_rx_packet_params(
            MESHTASTIC_PREAMBLE_LENGTH,
            false,                      // implicit header = false
            MAX_LORA_PAYLOAD_LEN as u8, // max payload
            true,                       // CRC on
            false,                      // IQ inversion off
            &modulation_params,
        )
        .expect("rx packet params derive only from already-validated modulation params");

    // SX1262 hardware RX duty cycling: the radio autonomously alternates a
    // short listen window with sleep, entirely on-chip, and only wakes the
    // host on an actual preamble detect. Matches every SX126x Meshtastic
    // board's `startReceiveDutyCycleAuto` — see this module's doc comment
    // and `rx_duty_cycle_params` for the derivation. `sender_preamble` is
    // MESHTASTIC_PREAMBLE_LENGTH (a stock node's TX preamble, which is also
    // what we transmit) — not necessarily the same thing as our own RX
    // preamble parameter above, even though today they're numerically equal.
    let rx_mode = rx_duty_cycle_params(
        MESHTASTIC_PREAMBLE_LENGTH,
        modem_cfg.spreading_factor,
        modem_cfg.bandwidth_hz,
    )
    .map(RxMode::DutyCycle)
    .unwrap_or(RxMode::Continuous);

    log::info!(
        "[LoRa] Entering RX mode ({}) at {} Hz...",
        if matches!(rx_mode, RxMode::DutyCycle(_)) {
            "duty-cycled"
        } else {
            "continuous"
        },
        frequency_hz
    );
    match lora
        .prepare_for_rx(rx_mode, &modulation_params, &rx_packet_params)
        .await
    {
        Ok(_) => log::info!("[LoRa] Ready - listening for Meshtastic packets"),
        Err(e) => {
            log::error!("[LoRa] FATAL: Failed to enter RX mode: {:?}", e);
            panic!("LoRa failed to enter RX mode");
        }
    }

    let mut rx_buffer = [0u8; MAX_LORA_PAYLOAD_LEN];
    let mut tx_count: u32 = 0;
    let mut rx_count: u32 = 0;

    // Channel utilization tracking (rolling 1-hour window, reported opportunistically —
    // see `report_channel_util` below for why this doesn't run on its own timer.
    let mut tx_airtime_ms: u64 = 0;
    let mut rx_airtime_ms: u64 = 0;
    let mut util_window_start = Instant::now();
    let mut last_util_report = Instant::now();

    // Minimum spacing between opportunistic reports piggybacked on the TX and
    // RX-success branches below — those branches already touch the radio for
    // other reasons, so reporting there costs nothing extra. 30s was fine as
    // an unconditional interval back when RX was continuous, where there was
    // nothing to disrupt; it isn't free to force anymore (see below), hence
    // it's now just a minimum gap between free opportunities rather than its
    // own timer.
    const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
    // Used only when the channel is silent enough that no TX/RX ever wakes
    // the loop naturally — see the `Either3::Third` arm below. Interrupting
    // an in-flight `lora.rx()` restarts the SX1262's hardware RX duty cycle
    // from scratch (`do_rx` unconditionally reprograms `SetRxDutyCycle`),
    // discarding whatever fraction of the current sleep window had already
    // elapsed and spending an extra SPI transaction + radio wake for a
    // report nobody's waiting on faster than the hourly telemetry broadcast
    // anyway — hence this is a much coarser interval than `HEARTBEAT_INTERVAL`.
    const SILENT_CHANNEL_REPORT_INTERVAL: Duration = Duration::from_secs(600);

    loop {
        match select3(
            tx_queue.receive(),
            lora.rx(&rx_packet_params, &mut rx_buffer),
            Timer::after(SILENT_CHANNEL_REPORT_INTERVAL),
        )
        .await
        {
            Either3::Third(_) => {
                // The channel's been silent long enough that nothing else
                // woke this loop — worth the RX restart to keep utilization
                // telemetry from going stale indefinitely.
                report_channel_util(
                    &mesh_in,
                    tx_airtime_ms,
                    rx_airtime_ms,
                    util_window_start,
                    rx_count,
                    tx_count,
                );
                last_util_report = Instant::now();
                if util_window_start.elapsed() >= Duration::from_secs(3600) {
                    tx_airtime_ms = 0;
                    rx_airtime_ms = 0;
                    util_window_start = Instant::now();
                }
                continue;
            }
            Either3::First(frame) => {
                tx_count += 1;
                log::info!("[LoRa] TX #{}: {} bytes", tx_count, frame.len);

                // CAD before transmit
                let mut cad_retries: u8 = 0;
                'cad: loop {
                    if cad_retries >= CAD_MAX_RETRIES {
                        log::warn!("[LoRa] TX #{}: CAD max retries, force TX", tx_count);
                        break 'cad;
                    }
                    match lora.prepare_for_cad(&modulation_params).await {
                        Ok(_) => {}
                        Err(e) => {
                            log::error!("[LoRa] TX #{}: CAD prepare failed: {:?}", tx_count, e);
                            break 'cad;
                        }
                    }
                    match lora.cad(&modulation_params).await {
                        Ok(false) => break 'cad, // Channel free
                        Ok(true) => {
                            cad_retries += 1;
                            // XOR node_num into tick count to break synchronization between nodes
                            let jitter = (Instant::now().as_ticks() ^ node_num as u64)
                                % CAD_BACKOFF_JITTER_MS;
                            Timer::after(Duration::from_millis(CAD_BACKOFF_BASE_MS + jitter)).await;
                        }
                        Err(e) => {
                            log::error!("[LoRa] TX #{}: CAD error: {:?}", tx_count, e);
                            break 'cad;
                        }
                    }
                }

                // Transmit
                match lora
                    .prepare_for_tx(
                        &modulation_params,
                        &mut tx_packet_params,
                        LORA_TX_POWER_DBM,
                        frame.as_bytes(),
                    )
                    .await
                {
                    Ok(()) => {
                        let tx_start = Instant::now();
                        match lora.tx().await {
                            Ok(()) => {
                                tx_airtime_ms += tx_start.elapsed().as_millis();
                                log::info!("[LoRa] TX #{}: complete", tx_count);
                            }
                            Err(e) => log::error!("[LoRa] TX #{}: FAILED: {:?}", tx_count, e),
                        }
                    }
                    Err(e) => log::error!("[LoRa] TX #{}: prepare failed: {:?}", tx_count, e),
                }

                // Return to RX. Already restarting the RX cycle here regardless
                // (TX itself required leaving RX mode), so this is a free point
                // to also report channel utilization if it's due — unlike the
                // silent-channel timeout branch, this doesn't cost an extra
                // disruption.
                if let Err(e) = lora
                    .prepare_for_rx(rx_mode, &modulation_params, &rx_packet_params)
                    .await
                {
                    log::error!("[LoRa] Failed to return to RX mode: {:?}", e);
                }
                if last_util_report.elapsed() >= HEARTBEAT_INTERVAL {
                    report_channel_util(
                        &mesh_in,
                        tx_airtime_ms,
                        rx_airtime_ms,
                        util_window_start,
                        rx_count,
                        tx_count,
                    );
                    last_util_report = Instant::now();
                    if util_window_start.elapsed() >= Duration::from_secs(3600) {
                        tx_airtime_ms = 0;
                        rx_airtime_ms = 0;
                        util_window_start = Instant::now();
                    }
                }
            }
            Either3::Second(Ok((len, status))) => {
                rx_count += 1;
                // Estimate RX airtime: ~1ms per byte at LongFast rates (rough approximation)
                rx_airtime_ms += len as u64;
                log::info!(
                    "[LoRa] RX #{}: {} bytes (RSSI: {} dBm, SNR: {} dB)",
                    rx_count,
                    len,
                    status.rssi,
                    status.snr
                );

                if let Some(frame) = RadioFrame::from_raw(&rx_buffer[..len as usize]) {
                    let metadata = RadioMetadata {
                        rssi: status.rssi,
                        snr: status.snr as i8,
                    };
                    if mesh_in
                        .try_send(MeshEvent::LoraRx(Box::new(frame), metadata))
                        .is_err()
                    {
                        log::error!("[LoRa] RX #{}: mesh_in full, DROPPED!", rx_count);
                    }
                } else {
                    log::warn!("[LoRa] RX #{}: invalid frame ({} bytes)", rx_count, len);
                }

                // A completed RX already leaves duty-cycle mode (the SX1262 is
                // "done"); the next loop iteration re-enters it via `lora.rx()`
                // regardless, so reporting here piggybacks on a restart that
                // was happening anyway rather than causing an extra one.
                if last_util_report.elapsed() >= HEARTBEAT_INTERVAL {
                    report_channel_util(
                        &mesh_in,
                        tx_airtime_ms,
                        rx_airtime_ms,
                        util_window_start,
                        rx_count,
                        tx_count,
                    );
                    last_util_report = Instant::now();
                    if util_window_start.elapsed() >= Duration::from_secs(3600) {
                        tx_airtime_ms = 0;
                        rx_airtime_ms = 0;
                        util_window_start = Instant::now();
                    }
                }
            }
            Either3::Second(Err(e)) => {
                log::warn!("[LoRa] RX error: {:?}", e);
                if let Err(e) = lora
                    .prepare_for_rx(rx_mode, &modulation_params, &rx_packet_params)
                    .await
                {
                    log::error!("[LoRa] Failed to recover RX mode: {:?}", e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tick is 15.625us == 1/64000 s; assert the exact conversion, not
    /// just "close enough" — the whole point of a fixed-point conversion is
    /// that it's exact.
    #[test]
    fn us_to_sx1262_ticks_matches_the_15_625us_tick_size() {
        assert_eq!(us_to_sx1262_ticks(15_625), 1000);
        assert_eq!(us_to_sx1262_ticks(0), 0);
        assert_eq!(us_to_sx1262_ticks(31_250), 2000);
    }

    /// Cross-check against the table in the plan (independently computed via
    /// a Python port of the same RadioLib algorithm), for our four presets
    /// with a 64-symbol sender preamble. Asserts within a couple percent
    /// rather than bit-exact, since the plan's table was in milliseconds.
    fn assert_duty_cycle_close(sf: u8, bw_hz: u32, expected_wake_ms: f64, expected_sleep_ms: f64) {
        let params =
            rx_duty_cycle_params(64, sf, bw_hz).expect("64-symbol preamble must duty cycle");
        let wake_ms = (params.rx_time as f64) * 15.625 / 1000.0;
        let sleep_ms = (params.sleep_time as f64) * 15.625 / 1000.0;
        let within_tolerance =
            |actual: f64, expected: f64| (actual - expected).abs() / expected < 0.02;
        assert!(
            within_tolerance(wake_ms, expected_wake_ms),
            "wake: got {wake_ms}ms, expected ~{expected_wake_ms}ms"
        );
        assert!(
            within_tolerance(sleep_ms, expected_sleep_ms),
            "sleep: got {sleep_ms}ms, expected ~{expected_sleep_ms}ms"
        );
    }

    #[test]
    fn longfast_duty_cycle_matches_expected_timings() {
        // SF11, BW250kHz
        assert_duty_cycle_close(11, 250_000, 73.7, 393.2);
    }

    #[test]
    fn longslow_duty_cycle_matches_expected_timings() {
        // SF12, BW125kHz
        assert_duty_cycle_close(12, 125_000, 294.9, 1572.9);
    }

    #[test]
    fn mediumfast_duty_cycle_matches_expected_timings() {
        // SF9, BW250kHz
        assert_duty_cycle_close(9, 250_000, 18.4, 98.3);
    }

    #[test]
    fn shortfast_duty_cycle_matches_expected_timings() {
        // SF7, BW250kHz
        assert_duty_cycle_close(7, 250_000, 4.9, 24.6);
    }

    #[test]
    fn sixteen_symbol_preamble_falls_back_to_continuous() {
        // 2 * min_symbols (8) == 16: RadioLib's own guard is `>`, so this is
        // the boundary case where upstream's default parameters produce a
        // zero-length sleep window and fall back to continuous RX.
        assert!(rx_duty_cycle_params(16, 11, 250_000).is_none());
    }

    #[test]
    fn very_short_preamble_falls_back_to_continuous() {
        assert!(rx_duty_cycle_params(8, 11, 250_000).is_none());
    }

    #[test]
    fn zero_bandwidth_does_not_panic() {
        assert!(rx_duty_cycle_params(64, 11, 0).is_none());
    }

    #[test]
    fn rx_time_always_covers_at_least_min_symbols_plus_one() {
        // RadioLib's guard (B): the wake window must be long enough to
        // reliably see minSymbols, regardless of how the preamble-based
        // term (A) computes. Allow one tick (15.625us) of slack for the
        // us<->tick round-trip truncation, since `rx_time` is quantized to
        // SX1262 ticks and the reconstructed `actual_wake_us` inherits that.
        const ONE_TICK_US: u32 = 16; // 15.625us rounded up
        for sf in 7..=12u8 {
            for bw in [125_000u32, 250_000, 500_000] {
                if let Some(params) = rx_duty_cycle_params(64, sf, bw) {
                    let symbol_length_us = (10_000u32 << sf) / (10 * (bw / 1000));
                    let min_wake_us = symbol_length_us * (RX_DUTY_CYCLE_MIN_SYMBOLS + 1);
                    let actual_wake_us = params.rx_time * 15625 / 1000;
                    assert!(
                        actual_wake_us + ONE_TICK_US >= min_wake_us,
                        "SF{sf} BW{bw}: wake {actual_wake_us}us < required {min_wake_us}us"
                    );
                }
            }
        }
    }
}
