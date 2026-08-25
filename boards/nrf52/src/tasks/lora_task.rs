//! Meshtastic LoRa task for SX1262 on the Wio-SX1262 for XIAO module.
//!
//! This file owns only nRF52-specific setup: SPI/GPIO construction, RF-switch
//! power, `lora_phy::LoRa::new()`, and the Meshtastic sync-word write. The
//! board-agnostic modem-config mapping and the TX/RX/CAD/channel-utilization
//! event loop live once in `meshtastenstein_core::drivers::lora_task_body`
//! and are shared with the ESP32 board's `lora_task` — see that module for
//! the actual radio logic.
//!
//! Two real differences from the ESP32 board's setup, both from this
//! module's hardware, not a design choice:
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

use embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice;
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
use log::info;
use lora_phy::{
    LoRa,
    iv::GenericSx126xInterfaceVariant,
    sx126x::{Config as Sx126xConfig, Sx126x, Sx1262, TcxoCtrlVoltage},
};
use meshtastenstein_core::{
    constants::*,
    domain::{packet::RadioFrame, radio_config::ModemConfig},
    drivers::{lora_task_body, sx1262_direct},
    inter_task::channels::MeshEvent,
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
