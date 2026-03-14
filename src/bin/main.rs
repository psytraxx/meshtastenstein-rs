//! Meshtastenstein: Meshtastic Protocol in Rust for ESP32-S3
//!
//! Entry point for Heltec WiFi LoRa V3 (ESP32-S3 + SX1262)

#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types, especially those \
    holding buffers for the duration of a data transfer."
)]
#![deny(clippy::large_stack_frames)]

use embassy_executor::Spawner;
use esp_alloc::heap_allocator;
use esp_backtrace as _;
use esp_hal::Config;
use esp_hal::analog::adc::{Adc, AdcConfig, Attenuation};
use esp_hal::clock::CpuClock;
use esp_hal::efuse::Efuse;
use esp_hal::gpio::Pin;
use esp_hal::rtc_cntl::{reset_reason, wakeup_cause};
use esp_hal::system::Cpu;
use esp_hal::timer::timg::{MwdtStage, TimerGroup};
use log::info;
use meshtastenstein::adapters::deep_sleep_adapter::DeepSleepAdapter;
use meshtastenstein::adapters::nvs_storage_adapter::NvsStorageAdapter;
use meshtastenstein::inter_task::Channels;
use meshtastenstein::tasks::ble_task::ble_task;
use meshtastenstein::tasks::lora_task::{LoraGpios, lora_task};
use meshtastenstein::tasks::mesh_task::MeshOrchestrator;
use meshtastenstein::tasks::{battery_task, led_task, watchdog_task};
use static_cell::StaticCell;

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

#[allow(clippy::large_stack_frames)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);
    esp_println::logger::init_logger_from_env();
    heap_allocator!(#[esp_hal::ram(reclaimed)] size: 73744);

    info!("========================================");
    info!("Meshtastenstein - Meshtastic in Rust");
    info!("Target: Heltec WiFi LoRa V3 (ESP32-S3)");
    info!("========================================");

    let wake_reason = wakeup_cause();
    let reset = reset_reason(Cpu::ProCpu);
    info!("[Boot] Reset: {:?}, Wake: {:?}", reset, wake_reason);
    let is_lora_wakeup = matches!(wake_reason, esp_hal::system::SleepSource::Ext0);

    // Timer and watchdog init
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let mut timg0_wdt = timg0.wdt;
    timg0_wdt.disable();
    esp_rtos::start(timg0.timer0);

    let timg1 = TimerGroup::new(peripherals.TIMG1);
    let mut wdt = timg1.wdt;
    wdt.set_timeout(MwdtStage::Stage0, esp_hal::time::Duration::from_secs(10));
    wdt.enable();
    info!("[Boot] HW watchdog enabled (10s)");

    // Radio init
    let radio_init = esp_radio::init().expect("Failed to initialize radio");
    let radio = RADIO.init(radio_init);

    // Channel init
    let ch = CHANNELS.init(Channels::new());

    // MAC address for node identity
    let mac = Efuse::read_base_mac_address();
    info!(
        "[Boot] MAC: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    // Spawn BLE task
    spawner
        .spawn(ble_task(
            radio,
            peripherals.BT,
            ch.ble_tx.receiver(),
            ch.ble_rx.sender(),
            ch.bat_level.receiver(),
            ch.conn_state.sender(),
            ch.disconn_cmd.receiver(),
            &ch.radio_stats,
        ))
        .expect("Failed to spawn BLE task");
    info!("[Boot] Task spawned: BLE");

    // Spawn LoRa task
    let lora_gpios = LoraGpios {
        cs: peripherals.GPIO8.degrade(),
        reset: peripherals.GPIO12.degrade(),
        dio1: peripherals.GPIO14.degrade(),
        busy: peripherals.GPIO13.degrade(),
        sck: peripherals.GPIO9.degrade(),
        miso: peripherals.GPIO11.degrade(),
        mosi: peripherals.GPIO10.degrade(),
    };
    spawner
        .spawn(lora_task(
            peripherals.SPI2,
            lora_gpios,
            ch.lora_tx.receiver(),
            ch.lora_rx.sender(),
            is_lora_wakeup,
        ))
        .expect("Failed to spawn LoRa task");
    info!("[Boot] Task spawned: LoRa");

    // Spawn LED task
    spawner
        .spawn(led_task(
            peripherals.GPIO35.degrade(),
            ch.led_cmd.receiver(),
        ))
        .expect("Failed to spawn LED task");
    info!("[Boot] Task spawned: LED");

    // Spawn Battery task
    let mut adc1_config = AdcConfig::new();
    let battery_pin = adc1_config.enable_pin(peripherals.GPIO1, Attenuation::_11dB);
    let adc1 = Adc::new(peripherals.ADC1, adc1_config);
    spawner
        .spawn(battery_task(
            adc1,
            battery_pin,
            5.1205,
            Some(peripherals.GPIO37.degrade()),
            ch.bat_level.sender(),
        ))
        .expect("Failed to spawn Battery task");
    info!("[Boot] Task spawned: Battery");

    // Spawn Watchdog task
    spawner
        .spawn(watchdog_task(wdt, &ch.activity, ch.disconn_cmd.sender()))
        .expect("Failed to spawn Watchdog task");
    info!("[Boot] Task spawned: Watchdog");

    // Initialize NVS storage and deep sleep adapters
    let _storage = STORAGE.init(NvsStorageAdapter::new(peripherals.FLASH));
    let _sleep = SLEEP.init(DeepSleepAdapter::new(peripherals.LPWR));

    // Create and run mesh orchestrator (runs on main task)
    let mut orchestrator = MeshOrchestrator::new(
        ch.lora_tx.sender(),
        ch.lora_rx.receiver(),
        ch.ble_tx.sender(),
        ch.ble_rx.receiver(),
        ch.conn_state.receiver(),
        ch.led_cmd.sender(),
        &ch.activity,
        &ch.radio_stats,
        &mac,
    );

    info!("========================================");
    info!("[Boot] BOOT COMPLETE - Starting mesh");
    info!("========================================");

    orchestrator.run().await
}

static RADIO: StaticCell<esp_radio::Controller<'static>> = StaticCell::new();
static CHANNELS: StaticCell<Channels> = StaticCell::new();
static STORAGE: StaticCell<NvsStorageAdapter<'static>> = StaticCell::new();
static SLEEP: StaticCell<DeepSleepAdapter<'static>> = StaticCell::new();
