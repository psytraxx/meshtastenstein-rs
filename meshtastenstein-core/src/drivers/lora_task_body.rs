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
use embassy_time::{Duration, Instant, Ticker, Timer};
use lora_phy::{
    DelayNs, LoRa, RxMode,
    mod_params::{Bandwidth, CodingRate, SpreadingFactor},
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

/// Bring an already-initialized `LoRa` radio into continuous RX and run the
/// TX/RX/CAD/channel-utilization event loop for the lifetime of the task.
/// Never returns.
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

    // Continuous RX for ROUTER role (no duty cycling)
    let rx_mode = RxMode::Continuous;

    log::info!(
        "[LoRa] Entering continuous RX mode at {} Hz...",
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
    let mut heartbeat = Ticker::every(Duration::from_secs(30));

    // Channel utilization tracking (rolling 1-hour window sampled every 30s)
    let mut tx_airtime_ms: u64 = 0;
    let mut rx_airtime_ms: u64 = 0;
    let mut util_window_start = Instant::now();

    loop {
        match select3(
            tx_queue.receive(),
            lora.rx(&rx_packet_params, &mut rx_buffer),
            heartbeat.next(),
        )
        .await
        {
            Either3::Third(_) => {
                // Compute and report channel utilization
                let elapsed_ms = util_window_start.elapsed().as_millis().max(1) as f32;
                let total_airtime_ms = (tx_airtime_ms + rx_airtime_ms) as f32;
                let channel_util_pct = (total_airtime_ms / elapsed_ms) * 100.0;
                let air_util_tx_pct = (tx_airtime_ms as f32 / elapsed_ms) * 100.0;
                let _ = mesh_in.try_send(MeshEvent::ChannelUtilUpdate(
                    channel_util_pct,
                    air_util_tx_pct,
                ));

                // Reset counters every hour
                if util_window_start.elapsed() >= Duration::from_secs(3600) {
                    tx_airtime_ms = 0;
                    rx_airtime_ms = 0;
                    util_window_start = Instant::now();
                }

                log::info!(
                    "[LoRa] RX loop alive: rx={} tx={} util={:.1}%",
                    rx_count,
                    tx_count,
                    channel_util_pct
                );
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

                // Return to RX
                if let Err(e) = lora
                    .prepare_for_rx(rx_mode, &modulation_params, &rx_packet_params)
                    .await
                {
                    log::error!("[LoRa] Failed to return to RX mode: {:?}", e);
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
