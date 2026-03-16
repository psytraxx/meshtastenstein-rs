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
use esp_hal::analog::adc::{Adc, AdcCalLine, AdcConfig, Attenuation};
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
use meshtastenstein::mesh::radio_config::{ModemConfig, ModemPreset, Region};
use meshtastenstein::tasks::ble_task::ble_task;
use meshtastenstein::tasks::lora_task::{LoraGpios, LoraParams, lora_task};
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
    heap_allocator!(#[unsafe(link_section = ".dram2_uninit")] size: 73744);

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

    // Initialize NVS storage early so we can load saved radio config for LoRa task
    let storage = STORAGE.init(NvsStorageAdapter::new(peripherals.FLASH));
    let sleep = SLEEP.init(DeepSleepAdapter::new(peripherals.LPWR));

    // Load persisted BLE bond (if any) so BLE task can restore it to the stack
    let initial_bond = storage.load_bond();

    // Derive LoRa modem config and frequency from saved config (or use region defaults)
    let (lora_modem_cfg, lora_frequency_hz) = if let Some(saved) = storage.load_config() {
        let region = Region::from_proto(saved.region);
        let modem_cfg = if saved.use_preset != 0 {
            ModemPreset::from_proto(saved.modem_preset).config()
        } else {
            ModemConfig {
                spreading_factor: saved.spread_factor,
                bandwidth_hz: saved.bandwidth_khz as u32 * 1000,
                coding_rate: saved.coding_rate,
            }
        };
        // channel_num=0 → compute from primary channel hash (name-only XOR % num_channels)
        // channel_num>0 → 1-indexed per proto spec ("between 1 and NUM_CHANNELS")
        let preset = if saved.use_preset != 0 {
            ModemPreset::from_proto(saved.modem_preset)
        } else {
            ModemPreset::default()
        };
        let channel_idx = if saved.channel_num != 0 {
            // channel_num is 1-indexed per proto spec ("between 1 and NUM_CHANNELS")
            (saved.channel_num as u32).saturating_sub(1)
        } else {
            // Hash-based: djb2(effectiveName) % numChannels
            region.default_channel_index(preset)
        };
        let freq = region.frequency_hz(modem_cfg.bandwidth_hz, channel_idx);
        info!(
            "[Boot] LoRa params from NVS: region={} use_preset={} SF={} BW={}Hz channel_num={} freq={}Hz",
            saved.region,
            saved.use_preset,
            modem_cfg.spreading_factor,
            modem_cfg.bandwidth_hz,
            channel_idx,
            freq
        );
        (modem_cfg, freq)
    } else {
        let preset = ModemPreset::default();
        let region = Region::Eu433;
        let modem_cfg = preset.config();
        let freq = preset.frequency_hz(region, region.default_channel_index(preset));
        (modem_cfg, freq)
    };

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
    let node_num = u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]);
    spawner
        .spawn(lora_task(
            peripherals.SPI2,
            lora_gpios,
            ch.lora_tx.receiver(),
            ch.lora_rx.sender(),
            LoraParams {
                is_wakeup: is_lora_wakeup,
                node_num,
                modem_cfg: lora_modem_cfg,
                frequency_hz: lora_frequency_hz,
            },
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
    // Use 6dB attenuation (covers 0–1750mV; pin voltage ~820mV at 4.2V/5.12 divider).
    // AdcCalLine uses eFuse calibration and returns readings in mV directly.
    let battery_pin =
        adc1_config.enable_pin_with_cal::<_, AdcCalLine<_>>(peripherals.GPIO1, Attenuation::_6dB);
    let adc1 = Adc::new(peripherals.ADC1, adc1_config);
    spawner
        .spawn(battery_task(
            adc1,
            battery_pin,
            meshtastenstein::constants::heltec_wifi_lora_v3::BATTERY_VOLTAGE_DIVIDER,
            Some(peripherals.GPIO37.degrade()),
            &ch.bat_level,
        ))
        .expect("Failed to spawn Battery task");
    info!("[Boot] Task spawned: Battery");

    // Spawn BLE task (done here, after storage init, so initial_bond is available)
    spawner
        .spawn(ble_task(
            radio,
            peripherals.BT,
            ch.ble_tx.receiver(),
            ch.ble_rx.sender(),
            ch.conn_state.sender(),
            ch.disconn_cmd.receiver(),
            &ch.radio_stats,
            initial_bond,
            ch.bond_save.sender(),
        ))
        .expect("Failed to spawn BLE task");
    info!("[Boot] Task spawned: BLE");

    // Spawn Watchdog task
    spawner
        .spawn(watchdog_task(
            wdt,
            &ch.activity,
            ch.disconn_cmd.sender(),
            sleep,
        ))
        .expect("Failed to spawn Watchdog task");
    info!("[Boot] Task spawned: Watchdog");

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
        storage,
        &ch.bat_level,
        ch.bond_save.receiver(),
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
