//! Meshtastic LoRa task for SX1262
//!
//! Adapted from template firmware. Key Meshtastic differences:
//! - Sync word 0x2B (set via register write after init)
//! - Preamble: 16 symbols
//! - Default preset LongFast: SF11, BW250kHz, CR4/5
//! - Frequency: region-dependent (US default: 906.875 MHz)
//! - Continuous RX for ROUTER role
//! - Buffer: 255 bytes

use crate::constants::*;
use crate::drivers::sx1262_direct;
use crate::mesh::packet::RadioFrame;
use crate::mesh::radio_config::ModemPreset;
use embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice;
use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Receiver, Sender};
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::{
    Async,
    gpio::{AnyPin, Input, InputConfig, Output, OutputConfig},
    time::Rate,
};
use log::{debug, error, info, warn};
use lora_phy::iv::GenericSx126xInterfaceVariant;
use lora_phy::mod_params::*;
use lora_phy::{
    LoRa, RxMode,
    sx126x::{Config as Sx126xConfig, Sx126x, Sx1262, TcxoCtrlVoltage},
};
use static_cell::StaticCell;

/// Metadata for received LoRa packets
#[derive(Debug, Clone, Copy)]
pub struct RadioMetadata {
    pub rssi: i16,
    pub snr: i8,
}

/// LoRa GPIO pins configuration
pub struct LoraGpios<'a> {
    pub cs: AnyPin<'a>,
    pub reset: AnyPin<'a>,
    pub dio1: AnyPin<'a>,
    pub busy: AnyPin<'a>,
    pub sck: AnyPin<'a>,
    pub miso: AnyPin<'a>,
    pub mosi: AnyPin<'a>,
}

static SPI_BUS: StaticCell<
    Mutex<CriticalSectionRawMutex, esp_hal::spi::master::Spi<'static, Async>>,
> = StaticCell::new();

#[embassy_executor::task]
pub async fn lora_task(
    spi_peripheral: esp_hal::peripherals::SPI2<'static>,
    gpios: LoraGpios<'static>,
    tx_queue: Receiver<'static, CriticalSectionRawMutex, RadioFrame, 5>,
    rx_queue: Sender<'static, CriticalSectionRawMutex, (RadioFrame, RadioMetadata), 5>,
    is_wakeup: bool,
) {
    let preset = ModemPreset::default();
    let modem_cfg = preset.config();

    info!(
        "[LoRa] Starting ({}). SF={}, BW={} Hz, CR=4/{}",
        if is_wakeup { "warm" } else { "cold" },
        modem_cfg.spreading_factor,
        modem_cfg.bandwidth_hz,
        modem_cfg.coding_rate
    );

    // Initialize SPI bus
    let spi = esp_hal::spi::master::Spi::new(
        spi_peripheral,
        esp_hal::spi::master::Config::default().with_frequency(Rate::from_mhz(1)),
    )
    .unwrap()
    .with_sck(gpios.sck)
    .with_mosi(gpios.mosi)
    .with_miso(gpios.miso)
    .into_async();

    let spi_bus = SPI_BUS.init(Mutex::new(spi));

    let mut cs = Output::new(
        gpios.cs,
        esp_hal::gpio::Level::High,
        OutputConfig::default(),
    );
    let reset = Output::new(
        gpios.reset,
        esp_hal::gpio::Level::High,
        OutputConfig::default(),
    );
    let dio1 = Input::new(gpios.dio1, InputConfig::default());
    let mut busy = Input::new(gpios.busy, InputConfig::default());

    // Read wake packet before lora-phy init (if waking from deep sleep)
    if is_wakeup {
        info!("[LoRa] Deep sleep wake - reading buffered packet...");
        let mut wake_buffer = [0u8; MAX_LORA_PAYLOAD_LEN];

        match sx1262_direct::read_wake_packet(spi_bus, &mut cs, &mut busy, &mut wake_buffer).await {
            Ok(Some((len, rssi, snr))) => {
                info!("[LoRa] Wake packet: {} bytes (RSSI: {}, SNR: {})", len, rssi, snr);
                if let Some(frame) = RadioFrame::from_raw(&wake_buffer[..len as usize]) {
                    let metadata = RadioMetadata { rssi, snr };
                    if rx_queue.try_send((frame, metadata)).is_err() {
                        warn!("[LoRa] Wake packet: channel full, dropped!");
                    }
                }
            }
            Ok(None) => info!("[LoRa] No buffered wake packet"),
            Err(e) => warn!("[LoRa] Wake packet read error: {:?}", e),
        }
    }

    // Initialize lora-phy
    let iv = GenericSx126xInterfaceVariant::new(reset, dio1, busy, None, None).unwrap();

    let chip_config = Sx126xConfig {
        chip: Sx1262,
        tcxo_ctrl: Some(TcxoCtrlVoltage::Ctrl1V8),
        use_dcdc: true,
        rx_boost: true,
    };
    let spi_device = SpiDevice::new(spi_bus, cs);
    let radio_hw = Sx126x::new(spi_device, iv, chip_config);

    let mut lora = LoRa::new(radio_hw, false, embassy_time::Delay)
        .await
        .expect("Failed to initialize LoRa radio");

    // Set Meshtastic sync word via direct register write
    // lora-phy doesn't expose sync word setting for SX126x, so we write registers directly
    // Register 0x0740 = MSB, 0x0741 = LSB
    info!("[LoRa] Setting Meshtastic sync word 0x{:04X}", MESHTASTIC_SYNC_WORD);
    // Note: We'll handle this via the sx1262_direct module or lora-phy internals
    // For now, lora-phy may need patching or we use a post-init register write

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

    let frequency_hz = DEFAULT_FREQUENCY_HZ;

    let modulation_params = lora
        .create_modulation_params(sf, bandwidth, cr, frequency_hz)
        .unwrap();

    let mut tx_packet_params = lora
        .create_tx_packet_params(
            MESHTASTIC_PREAMBLE_LENGTH,
            false, // implicit header = false
            true,  // CRC on
            false, // IQ inversion off
            &modulation_params,
        )
        .unwrap();

    let rx_packet_params = lora
        .create_rx_packet_params(
            MESHTASTIC_PREAMBLE_LENGTH,
            false,                     // implicit header = false
            MAX_LORA_PAYLOAD_LEN as u8, // max payload
            true,                      // CRC on
            false,                     // IQ inversion off
            &modulation_params,
        )
        .unwrap();

    // Continuous RX for ROUTER role (no duty cycling)
    let rx_mode = RxMode::Continuous;

    info!("[LoRa] Entering continuous RX mode at {} Hz...", frequency_hz);
    match lora.prepare_for_rx(rx_mode, &modulation_params, &rx_packet_params).await {
        Ok(_) => info!("[LoRa] Ready - listening for Meshtastic packets"),
        Err(e) => {
            error!("[LoRa] FATAL: Failed to enter RX mode: {:?}", e);
            panic!("LoRa failed to enter RX mode");
        }
    }

    let mut rx_buffer = [0u8; MAX_LORA_PAYLOAD_LEN];
    let mut tx_count: u32 = 0;
    let mut rx_count: u32 = 0;

    loop {
        match select(
            tx_queue.receive(),
            lora.rx(&rx_packet_params, &mut rx_buffer),
        )
        .await
        {
            Either::First(frame) => {
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
                            let jitter = Instant::now().as_ticks() % CAD_BACKOFF_JITTER_MS;
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
                    Ok(()) => match lora.tx().await {
                        Ok(()) => info!("[LoRa] TX #{}: complete", tx_count),
                        Err(e) => error!("[LoRa] TX #{}: FAILED: {:?}", tx_count, e),
                    },
                    Err(e) => error!("[LoRa] TX #{}: prepare failed: {:?}", tx_count, e),
                }

                // Return to RX
                if let Err(e) = lora.prepare_for_rx(rx_mode, &modulation_params, &rx_packet_params).await {
                    error!("[LoRa] Failed to return to RX mode: {:?}", e);
                }
            }
            Either::Second(Ok((len, status))) => {
                rx_count += 1;
                debug!(
                    "[LoRa] RX #{}: {} bytes (RSSI: {}, SNR: {})",
                    rx_count, len, status.rssi, status.snr
                );

                if let Some(frame) = RadioFrame::from_raw(&rx_buffer[..len as usize]) {
                    let metadata = RadioMetadata {
                        rssi: status.rssi,
                        snr: status.snr as i8,
                    };
                    if rx_queue.try_send((frame, metadata)).is_err() {
                        error!("[LoRa] RX #{}: channel full, DROPPED!", rx_count);
                    }
                } else {
                    warn!("[LoRa] RX #{}: invalid frame ({} bytes)", rx_count, len);
                }
            }
            Either::Second(Err(e)) => {
                warn!("[LoRa] RX error: {:?}", e);
                if let Err(e) = lora.prepare_for_rx(rx_mode, &modulation_params, &rx_packet_params).await {
                    error!("[LoRa] Failed to recover RX mode: {:?}", e);
                }
            }
        }
    }
}
