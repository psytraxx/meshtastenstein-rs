//! Meshtastic LoRa task for SX1262
//!
//! Adapted from template firmware. Key Meshtastic differences:
//! - Sync word 0x2B (set via register write after init)
//! - Preamble: 64 symbols (`MESHTASTIC_PREAMBLE_LENGTH`) — a deliberate
//!   divergence from upstream Meshtastic's own 16-symbol preamble (itself
//!   already raised from LoRa's 8-symbol default). See the constant's doc
//!   comment for the trade-off.
//! - Default preset LongFast: SF11, BW250kHz, CR4/5
//! - Frequency: region-dependent, computed by `DeviceState::lora_params()`
//!   (EU_433 + LongFast default: 433.875 MHz, slot 3)
//! - Continuous RX for ROUTER role
//! - Buffer: 255 bytes
//!
//! This file owns only ESP32-specific setup: SPI/GPIO construction, the
//! deep-sleep wake-packet read, `lora_phy::LoRa::new()`, and the Meshtastic
//! sync-word write. The board-agnostic modem-config mapping and the
//! TX/RX/CAD/channel-utilization event loop live once in
//! `meshtastenstein_core::drivers::lora_task_body` and are shared with the
//! nRF52 board's `lora_task` — see that module for the actual radio logic.

extern crate alloc;
use alloc::boxed::Box;
use embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice;
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    channel::{Receiver, Sender},
    mutex::Mutex,
};
use esp_hal::{
    Async,
    gpio::{AnyPin, Input, InputConfig, Output, OutputConfig},
    time::Rate,
};
use log::{info, warn};
use lora_phy::{LoRa, iv::GenericSx126xInterfaceVariant, sx126x::Sx126x};
use meshtastenstein_core::{
    constants::*,
    domain::{packet::RadioFrame, radio_config::ModemConfig},
    drivers::{lora_task_body, sx1262_direct},
    inter_task::channels::{MeshEvent, RadioMetadata},
};
use static_cell::StaticCell;

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

/// Boot-time LoRa radio parameters
pub struct LoraParams {
    pub is_wakeup: bool,
    pub node_num: u32,
    pub modem_cfg: ModemConfig,
    pub frequency_hz: u32,
}

static SPI_BUS: StaticCell<
    Mutex<CriticalSectionRawMutex, esp_hal::spi::master::Spi<'static, Async>>,
> = StaticCell::new();

#[embassy_executor::task]
pub async fn lora_task(
    spi_peripheral: esp_hal::peripherals::SPI2<'static>,
    gpios: LoraGpios<'static>,
    tx_queue: Receiver<'static, CriticalSectionRawMutex, RadioFrame, 5>,
    mesh_in: Sender<'static, CriticalSectionRawMutex, MeshEvent, 8>,
    params: LoraParams,
) {
    let LoraParams {
        is_wakeup,
        node_num,
        modem_cfg,
        frequency_hz,
    } = params;
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
                info!(
                    "[LoRa] Wake packet: {} bytes (RSSI: {}, SNR: {})",
                    len, rssi, snr
                );
                // Log first 16 bytes (OTA header) so we can see dest/sender/id/channel
                let raw = &wake_buffer[..len as usize];
                if raw.len() >= 16 {
                    info!(
                        "[LoRa] Wake hdr: dst={:08x} src={:08x} id={:08x} ch=0x{:02x} next=0x{:02x} relay=0x{:02x}",
                        u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
                        u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]),
                        u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]),
                        raw[13],
                        raw[14],
                        raw[15],
                    );
                } else {
                    warn!(
                        "[LoRa] Wake packet too short for header: {} bytes",
                        raw.len()
                    );
                }
                match RadioFrame::from_raw(raw) {
                    Some(frame) => {
                        let metadata = RadioMetadata { rssi, snr };
                        if mesh_in
                            .try_send(MeshEvent::LoraRx(Box::new(frame), metadata))
                            .is_err()
                        {
                            warn!("[LoRa] Wake packet: mesh_in full, dropped!");
                        } else {
                            info!("[LoRa] Wake packet queued to mesh_in OK");
                        }
                    }
                    None => warn!(
                        "[LoRa] Wake packet: RadioFrame::from_raw failed (len={}, HEADER_SIZE=16, MAX=255)",
                        raw.len()
                    ),
                }
            }
            Ok(None) => info!("[LoRa] No buffered wake packet"),
            Err(e) => warn!("[LoRa] Wake packet read error: {:?}", e),
        }
    }

    // Initialize lora-phy
    let iv = GenericSx126xInterfaceVariant::new(reset, dio1, busy, None, None).unwrap();

    let chip_config = meshtastenstein_core::drivers::lora_task_body::meshtastic_sx1262_config();
    let spi_device = SpiDevice::new(spi_bus, cs);
    let radio_hw = Sx126x::new(spi_device, iv, chip_config);

    let mut lora = LoRa::new(radio_hw, false, embassy_time::Delay)
        .await
        .expect("Failed to initialize LoRa radio");

    // Write Meshtastic sync word (0x2B) to SX1262 registers after lora-phy init.
    // lora-phy sets the standard LoRaWAN sync word during init, but Meshtastic uses 0x2B.
    // We steal the CS and BUSY pins temporarily to do a direct SPI register write.
    {
        // SAFETY: We steal the CS and BUSY pins here to perform a one-shot direct SPI
        // register write. This is sound because:
        // 1. All operations in this block are sequential within a single Embassy task.
        // 2. The lora-phy `LoRa` instance is not used while the stolen pins exist.
        // 3. `stolen_cs` and `stolen_busy` are dropped at the end of this block,
        //    releasing the duplicate pin references before lora-phy resumes use.
        let mut stolen_cs = Output::new(
            unsafe { AnyPin::steal(crate::constants::heltec_wifi_lora_v3::LORA_SS) },
            esp_hal::gpio::Level::High,
            OutputConfig::default(),
        );
        let mut stolen_busy = Input::new(
            unsafe { AnyPin::steal(crate::constants::heltec_wifi_lora_v3::LORA_BUSY) },
            InputConfig::default(),
        );
        sx1262_direct::write_sync_word(
            spi_bus,
            &mut stolen_cs,
            &mut stolen_busy,
            SX1262_SYNC_WORD_MSB,
            SX1262_SYNC_WORD_LSB,
        )
        .await
        .expect("Failed to set Meshtastic sync word");
        sx1262_direct::write_rx_sensitivity_patch(spi_bus, &mut stolen_cs, &mut stolen_busy)
            .await
            .expect("Failed to apply RX-sensitivity patch");
        sx1262_direct::write_current_limit(spi_bus, &mut stolen_cs, &mut stolen_busy)
            .await
            .expect("Failed to set current limit");
        info!(
            "[LoRa] Meshtastic sync word 0x{:04X} written to registers",
            MESHTASTIC_SYNC_WORD
        );
        // stolen_cs and stolen_busy are dropped here, releasing the duplicate pin references
    }

    lora_task_body::run(
        &mut lora,
        &modem_cfg,
        frequency_hz,
        node_num,
        tx_queue,
        mesh_in,
    )
    .await
}
