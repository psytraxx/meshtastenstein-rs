//! Meshtastic LoRa task for SX1262 on the Wio-SX1262 for XIAO module.
//!
//! Structurally mirrors the ESP32 board's `lora_task`. Two real differences:
//! - **RF switch**: this module exposes an RXEN pin the Heltec board doesn't
//!   have. DIO2 already drives the TX/RX direction automatically (lora-phy's
//!   default `Sx1262` variant sets `use_dio2_as_rfswitch()`), but RXEN
//!   separately gates power to the switch chip itself — for either direction,
//!   not just RX, despite the name. Driven high once at startup and left
//!   there; this firmware doesn't attempt the sleep-time power saving from
//!   dropping it low between transactions.
//! - **No wake-from-deep-sleep path**: this board has no `Sleep` port
//!   implementation yet (see CLAUDE.md), so there's no buffered-wake-packet
//!   read before init — this task always does a cold init.
//!
//! Sync word 0x2B is written after lora-phy's init (which resets the chip and
//! its default sync word), same ordering as the ESP32 board. The mechanism
//! differs: embassy-nrf's `Peri::clone_unchecked` duplicates the CS/BUSY
//! handles before lora-phy takes ownership of the originals, rather than
//! `esp_hal`'s `AnyPin::steal()` reclaiming them afterward — same safety
//! argument (sequential, non-overlapping use), different escape hatch.

extern crate alloc;
use alloc::boxed::Box;
use embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice;
use embassy_futures::select::{Either3, select3};
use embassy_nrf::{
    Peri,
    gpio::{AnyPin, Input, Level, Output, OutputDrive, Pull},
    peripherals::SPI3,
    spim::{self, Spim},
};
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    channel::{Receiver, Sender},
    mutex::Mutex,
};
use embassy_time::{Duration, Instant, Ticker, Timer};
use log::{error, info, warn};
use lora_phy::{
    LoRa, RxMode,
    iv::GenericSx126xInterfaceVariant,
    mod_params::*,
    sx126x::{Config as Sx126xConfig, Sx126x, Sx1262, TcxoCtrlVoltage},
};
use meshtastenstein_core::{
    constants::*,
    domain::{packet::RadioFrame, radio_config::ModemConfig},
    drivers::sx1262_direct,
    inter_task::channels::{MeshEvent, RadioMetadata},
};
use static_cell::StaticCell;

/// LoRa GPIO pins configuration
pub struct LoraGpios {
    pub cs: Peri<'static, AnyPin>,
    pub reset: Peri<'static, AnyPin>,
    pub dio1: Peri<'static, AnyPin>,
    pub busy: Peri<'static, AnyPin>,
    pub rxen: Peri<'static, AnyPin>,
    pub sck: Peri<'static, embassy_nrf::peripherals::P1_13>,
    pub miso: Peri<'static, embassy_nrf::peripherals::P1_14>,
    pub mosi: Peri<'static, embassy_nrf::peripherals::P1_15>,
}

/// Boot-time LoRa radio parameters
pub struct LoraParams {
    pub node_num: u32,
    pub modem_cfg: ModemConfig,
    pub frequency_hz: u32,
}

static SPI_BUS: StaticCell<Mutex<CriticalSectionRawMutex, Spim<'static>>> = StaticCell::new();

#[embassy_executor::task]
pub async fn lora_task(
    spi_peripheral: Peri<'static, SPI3>,
    gpios: LoraGpios,
    tx_queue: Receiver<'static, CriticalSectionRawMutex, RadioFrame, 5>,
    mesh_in: Sender<'static, CriticalSectionRawMutex, MeshEvent, 8>,
    params: LoraParams,
) {
    let LoraParams {
        node_num,
        modem_cfg,
        frequency_hz,
    } = params;
    info!(
        "[LoRa] Starting (cold). SF={}, BW={} Hz, CR=4/{}",
        modem_cfg.spreading_factor, modem_cfg.bandwidth_hz, modem_cfg.coding_rate
    );

    // RF switch power: gates the antenna switch for both TX and RX (despite
    // the RXEN name) — direction itself is DIO2, handled automatically by
    // lora-phy below. Held high for the task's lifetime.
    let _rxen = Output::new(gpios.rxen, Level::High, OutputDrive::Standard);

    // Initialize SPI bus
    let mut spim_config = spim::Config::default();
    spim_config.frequency = spim::Frequency::M1;
    let spi = Spim::new(
        spi_peripheral,
        crate::Irqs,
        gpios.sck,
        gpios.miso,
        gpios.mosi,
        spim_config,
    );

    let spi_bus = SPI_BUS.init(Mutex::new(spi));

    let LoraGpios {
        cs,
        reset,
        dio1,
        busy,
        ..
    } = gpios;

    // Duplicate the CS/BUSY peripheral handles before handing the originals
    // to lora-phy, so we can write the Meshtastic sync word directly to the
    // SX1262 registers afterward. `LoRa::new()` below performs a hardware
    // reset of the chip internally (toggling the RESET pin), which resets the
    // sync word to lora-phy's own default — so this write has to happen
    // *after* lora-phy's init, not before, and by then lora-phy already owns
    // the "real" `cs`/`busy` for good.
    //
    // SAFETY: `clone_unchecked` requires the two handles never be used
    // concurrently. That holds here: the cloned pair is used exactly once,
    // for the sequential SPI transaction in `write_sync_word` below, which
    // completes and is dropped before any `lora.*()` call touches the
    // originals — mirrors the ESP32 board's `AnyPin::steal()` for the same
    // operation, just via embassy-nrf's equivalent escape hatch.
    let (cs_sync, busy_sync) = unsafe { (cs.clone_unchecked(), busy.clone_unchecked()) };

    let cs_pin = Output::new(cs, Level::High, OutputDrive::Standard);
    let reset_pin = Output::new(reset, Level::High, OutputDrive::Standard);
    let dio1_pin = Input::new(dio1, Pull::None);
    let busy_pin = Input::new(busy, Pull::None);

    let iv = GenericSx126xInterfaceVariant::new(reset_pin, dio1_pin, busy_pin, None, None).unwrap();

    let chip_config = Sx126xConfig {
        chip: Sx1262,
        tcxo_ctrl: Some(TcxoCtrlVoltage::Ctrl1V8),
        use_dcdc: true,
        rx_boost: true,
    };
    let spi_device = SpiDevice::new(spi_bus, cs_pin);
    let radio_hw = Sx126x::new(spi_device, iv, chip_config);

    let mut lora = LoRa::new(radio_hw, false, embassy_time::Delay)
        .await
        .expect("Failed to initialize LoRa radio");

    // Write the Meshtastic sync word (0x2B) now that lora-phy's own reset has
    // already happened, using the duplicate CS/BUSY handles from above.
    {
        let mut cs_for_sync = Output::new(cs_sync, Level::High, OutputDrive::Standard);
        let mut busy_for_sync = Input::new(busy_sync, Pull::None);
        sx1262_direct::write_sync_word(
            spi_bus,
            &mut cs_for_sync,
            &mut busy_for_sync,
            SX1262_SYNC_WORD_MSB,
            SX1262_SYNC_WORD_LSB,
        )
        .await
        .expect("Failed to set Meshtastic sync word");
        // cs_for_sync/busy_for_sync are dropped here, before `lora` is used
        // for anything, so the duplicate handles never overlap in use with
        // the originals lora-phy owns.
    }
    info!(
        "[LoRa] Meshtastic sync word 0x{:04X} written to registers",
        MESHTASTIC_SYNC_WORD
    );

    info!("[LoRa] Radio initialized, configuring modulation...");

    // Map bandwidth to lora-phy enum
    let bandwidth = match modem_cfg.bandwidth_hz {
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
    };

    // Map spreading factor
    let sf = match modem_cfg.spreading_factor {
        5 => SpreadingFactor::_5,
        6 => SpreadingFactor::_6,
        7 => SpreadingFactor::_7,
        8 => SpreadingFactor::_8,
        9 => SpreadingFactor::_9,
        10 => SpreadingFactor::_10,
        11 => SpreadingFactor::_11,
        12 => SpreadingFactor::_12,
        _ => SpreadingFactor::_11,
    };

    // Map coding rate
    let cr = match modem_cfg.coding_rate {
        5 => CodingRate::_4_5,
        6 => CodingRate::_4_6,
        7 => CodingRate::_4_7,
        8 => CodingRate::_4_8,
        _ => CodingRate::_4_5,
    };

    // sf/bandwidth/cr are already clamped to valid lora-phy enum values above,
    // but the combination can still be rejected — e.g. lora-phy refuses a
    // 250/500 kHz bandwidth below 400 MHz, which the ITU 144-148 MHz amateur
    // regions fall under. Fall back to Meshtastic's own default (LongFast:
    // SF11/BW250kHz/CR4-5) rather than panic the radio task.
    let modulation_params = lora
        .create_modulation_params(sf, bandwidth, cr, frequency_hz)
        .unwrap_or_else(|e| {
            error!(
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
        });

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

    info!(
        "[LoRa] Entering continuous RX mode at {} Hz...",
        frequency_hz
    );
    match lora
        .prepare_for_rx(rx_mode, &modulation_params, &rx_packet_params)
        .await
    {
        Ok(_) => info!("[LoRa] Ready - listening for Meshtastic packets"),
        Err(e) => {
            error!("[LoRa] FATAL: Failed to enter RX mode: {:?}", e);
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

                info!(
                    "[LoRa] RX loop alive: rx={} tx={} util={:.1}%",
                    rx_count, tx_count, channel_util_pct
                );
                continue;
            }
            Either3::First(frame) => {
                tx_count += 1;
                info!("[LoRa] TX #{}: {} bytes", tx_count, frame.len);

                // CAD before transmit
                let mut cad_retries: u8 = 0;
                'cad: loop {
                    if cad_retries >= CAD_MAX_RETRIES {
                        warn!("[LoRa] TX #{}: CAD max retries, force TX", tx_count);
                        break 'cad;
                    }
                    match lora.prepare_for_cad(&modulation_params).await {
                        Ok(_) => {}
                        Err(e) => {
                            error!("[LoRa] TX #{}: CAD prepare failed: {:?}", tx_count, e);
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
                            error!("[LoRa] TX #{}: CAD error: {:?}", tx_count, e);
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
                                info!("[LoRa] TX #{}: complete", tx_count);
                            }
                            Err(e) => error!("[LoRa] TX #{}: FAILED: {:?}", tx_count, e),
                        }
                    }
                    Err(e) => error!("[LoRa] TX #{}: prepare failed: {:?}", tx_count, e),
                }

                // Return to RX
                if let Err(e) = lora
                    .prepare_for_rx(rx_mode, &modulation_params, &rx_packet_params)
                    .await
                {
                    error!("[LoRa] Failed to return to RX mode: {:?}", e);
                }
            }
            Either3::Second(Ok((len, status))) => {
                rx_count += 1;
                // Estimate RX airtime: ~1ms per byte at LongFast rates (rough approximation)
                rx_airtime_ms += len as u64;
                info!(
                    "[LoRa] RX #{}: {} bytes (RSSI: {} dBm, SNR: {} dB)",
                    rx_count, len, status.rssi, status.snr
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
                        error!("[LoRa] RX #{}: mesh_in full, DROPPED!", rx_count);
                    }
                } else {
                    warn!("[LoRa] RX #{}: invalid frame ({} bytes)", rx_count, len);
                }
            }
            Either3::Second(Err(e)) => {
                warn!("[LoRa] RX error: {:?}", e);
                if let Err(e) = lora
                    .prepare_for_rx(rx_mode, &modulation_params, &rx_packet_params)
                    .await
                {
                    error!("[LoRa] Failed to recover RX mode: {:?}", e);
                }
            }
        }
    }
}
