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
use esp_hal::{
    Config,
    analog::adc::{Adc, AdcCalLine, AdcConfig, Attenuation},
    clock::CpuClock,
    gpio::Pin,
    rng::Rng,
    rtc_cntl::{reset_reason, wakeup_cause},
    system::Cpu,
    timer::timg::{MwdtStage, TimerGroup},
};
use log::info;
use meshtastenstein_core::{
    domain::{crypto_pkc::keypair_from_seed, device::DeviceState},
    inter_task::Channels,
    ports::{ConfigStorage, Identity},
    tasks::mesh_task::MeshOrchestrator,
};
use static_cell::StaticCell;

use crate::{
    adapters::{
        deep_sleep_adapter::DeepSleepAdapter, esp_entropy_adapter::EspEntropyAdapter,
        esp_identity_adapter::EspIdentityAdapter, esp_reboot_adapter::EspRebootAdapter,
        nvs_storage_adapter::NvsStorageAdapter,
    },
    tasks::{
        battery_task,
        ble_task::ble_task,
        esp_led_task,
        lora_task::{LoraGpios, LoraParams, lora_task},
        watchdog_task,
    },
};

extern crate alloc;

mod adapters;
mod constants;
mod tasks;

esp_bootloader_esp_idf::esp_app_desc!();

#[allow(
    clippy::large_stack_frames,
    reason = "it's not unusual to allocate larger buffers etc. in main"
)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = Config::default().with_cpu_clock(CpuClock::max());
    let mut peripherals = esp_hal::init(config);
    esp_println::logger::init_logger_from_env();
    heap_allocator!(#[unsafe(link_section = ".dram2_uninit")] size: 73744);

    // VEXT (active low) powers the OLED display and the LoRa antenna boost —
    // not the SX1262 core supply. Upstream drives it on at boot
    // (`digitalWrite(VEXT_ENABLE, VEXT_ON_VALUE)`, VEXT_ON_VALUE = LOW);
    // without this the antenna boost is never enabled, costing RX
    // sensitivity. `main` never returns, so binding this for its scope holds
    // the rail on for the program's entire lifetime.
    let _vext = esp_hal::gpio::Output::new(
        peripherals.GPIO36.reborrow(),
        esp_hal::gpio::Level::Low,
        esp_hal::gpio::OutputConfig::default(),
    );

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
    let sw_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_interrupt.software_interrupt0);

    let timg1 = TimerGroup::new(peripherals.TIMG1);
    let mut wdt = timg1.wdt;
    // 90s, matching upstream's APP_WATCHDOG_SECS: its comment explains the
    // wait-to-sleep timeout for shutting down radios is 30s, so the
    // watchdog needs enough margin above that to avoid false positives —
    // the same admin-shutdown wait watchdog_task.rs feeds through below.
    wdt.set_timeout(MwdtStage::Stage0, esp_hal::time::Duration::from_secs(90));
    wdt.enable();
    info!("[Boot] HW watchdog enabled (90s)");

    // Channel init
    let ch = CHANNELS.init(Channels::new());

    // MAC address for node identity via Identity port
    let identity = EspIdentityAdapter;
    let mac = match identity.mac_address() {
        Ok(mac) => {
            info!(
                "[Boot] MAC: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
                mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
            );
            mac
        }
        Err(e) => {
            info!("[Boot] Failed to get MAC address: {}", e);
            panic!("Cannot continue without MAC address");
        }
    };

    // Initialize NVS storage early so we can load saved radio config for LoRa task
    let storage = STORAGE.init(NvsStorageAdapter::new(peripherals.FLASH));
    let sleep = SLEEP.init(DeepSleepAdapter::new(peripherals.LPWR));

    // Load persisted BLE bond (if any) so BLE task can restore it to the stack
    let initial_bond = storage.load_bond().await;

    // Initialize device state and apply saved config via Port trait
    let mut device = DeviceState::new(&mac);
    storage.load_state(&mut device).await;

    // Load or generate the X25519 PKC keypair (Phase 2 G2).
    // On first boot (or after factory reset) we generate a fresh 32-byte seed
    // from the hardware TRNG and persist both halves so the device keeps the
    // same identity across reboots.
    let pkc_keypair: ([u8; 32], [u8; 32]) = match storage.load_pkc_keypair().await {
        Some(pair) => {
            info!("[Boot] PKC keypair loaded from flash");
            pair
        }
        None => {
            let mut seed = [0u8; 32];
            Rng::new().read(&mut seed);
            let (secret, public) = keypair_from_seed(seed);
            let priv_bytes: [u8; 32] = secret.to_bytes();
            let pub_bytes: [u8; 32] = public.to_bytes();
            // A failed save here isn't safe to continue past: every reboot
            // would silently generate a new identity, breaking every peer's
            // ability to decrypt direct messages to this node with no visible
            // symptom beyond "DMs stopped working." Panic instead so the
            // watchdog-triggered restart and the failure are both visible.
            storage
                .save_pkc_keypair(&priv_bytes, &pub_bytes)
                .await
                .expect(
                    "Failed to persist PKC keypair — cannot continue without a stable identity",
                );
            info!("[Boot] PKC keypair generated and saved");
            (priv_bytes, pub_bytes)
        }
    };

    // Derive LoRa modem config and frequency from device state (Core logic)
    let (lora_modem_cfg, lora_frequency_hz) = device.lora_params();

    info!(
        "[Boot] LoRa params: SF={} BW={}Hz freq={}Hz",
        lora_modem_cfg.spreading_factor, lora_modem_cfg.bandwidth_hz, lora_frequency_hz
    );

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
    spawner.spawn(
        lora_task(
            peripherals.SPI2,
            lora_gpios,
            ch.lora_tx.receiver(),
            ch.mesh_in.sender(),
            LoraParams {
                is_wakeup: is_lora_wakeup,
                node_num,
                modem_cfg: lora_modem_cfg,
                frequency_hz: lora_frequency_hz,
            },
        )
        .expect("Failed to spawn LoRa task"),
    );
    info!("[Boot] Task spawned: LoRa");

    // Spawn LED task
    let led_pin = esp_hal::gpio::Output::new(
        peripherals.GPIO35.degrade(),
        esp_hal::gpio::Level::Low,
        esp_hal::gpio::OutputConfig::default(),
    );
    spawner.spawn(esp_led_task(led_pin, ch.led_cmd.receiver()).expect("Failed to spawn LED task"));
    info!("[Boot] Task spawned: LED");

    // Spawn Battery task
    let mut adc1_config = AdcConfig::new();
    // 2.5dB attenuation, matching upstream's ADC_ATTENUATION for this exact
    // board (variant.h: "lower dB for high resistance voltage divider").
    // Gives a ~1250mV full scale, putting the ~820mV operating point
    // (4.2V / 5.12 divider) at ~66% of range instead of 6dB's ~47% —
    // better effective resolution, and AdcCalLine's eFuse calibration is
    // per-attenuation, so this also matches upstream's calibration curve
    // rather than a differently-calibrated one at 6dB.
    let battery_pin =
        adc1_config.enable_pin_with_cal::<_, AdcCalLine<_>>(peripherals.GPIO1, Attenuation::_2p5dB);
    let adc1 = Adc::new(peripherals.ADC1, adc1_config);
    spawner.spawn(
        battery_task(
            adc1,
            battery_pin,
            crate::constants::heltec_wifi_lora_v3::BATTERY_VOLTAGE_DIVIDER,
            Some(peripherals.GPIO37.degrade()),
            &ch.bat_level,
            ch.mesh_in.sender(),
        )
        .expect("Failed to spawn Battery task"),
    );
    info!("[Boot] Task spawned: Battery");

    // Spawn BLE task (done here, after storage init, so initial_bond is available)
    spawner
        .spawn(ble_task(peripherals.BT, ch, initial_bond, mac).expect("Failed to spawn BLE task"));
    info!("[Boot] Task spawned: BLE");

    // Spawn Watchdog task
    spawner.spawn(
        watchdog_task(
            wdt,
            &ch.activity,
            ch.disconn_cmd.sender(),
            sleep,
            &ch.bat_level,
            &ch.shutdown_cmd,
        )
        .expect("Failed to spawn Watchdog task"),
    );
    info!("[Boot] Task spawned: Watchdog");

    // Create and run mesh orchestrator (runs on main task)
    let mut orchestrator = MeshOrchestrator::new(
        ch,
        &mac,
        storage,
        pkc_keypair,
        EspRebootAdapter,
        EspEntropyAdapter,
    )
    .await;

    info!("========================================");
    info!("[Boot] BOOT COMPLETE - Starting mesh");
    info!("========================================");

    orchestrator.run().await
}

static CHANNELS: StaticCell<Channels> = StaticCell::new();
static STORAGE: StaticCell<NvsStorageAdapter<'static>> = StaticCell::new();
static SLEEP: StaticCell<DeepSleepAdapter<'static>> = StaticCell::new();
