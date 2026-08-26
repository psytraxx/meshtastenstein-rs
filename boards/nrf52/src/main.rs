//! Meshtastenstein: Meshtastic Protocol in Rust for nRF52840
//!
//! Entry point for Seeed XIAO nRF52840 + Wio-SX1262 for XIAO.
//!
//! Feature-complete (radio, BLE, flash storage, mesh orchestrator, battery,
//! watchdog) but **never run on real hardware**.

#![no_std]
#![no_main]
#![deny(clippy::large_stack_frames)]

use embassy_executor::Spawner;
use embassy_nrf::{
    bind_interrupts,
    gpio::{Level, Output, OutputDrive},
    peripherals::RNG,
    rng, wdt,
};
use log::info;
use meshtastenstein_core::{
    constants::BLE_DEVICE_NAME_PREFIX,
    domain::{crypto_pkc::keypair_from_seed, device::DeviceState},
    inter_task::Channels,
    ports::{ConfigStorage, Identity},
    tasks::mesh_task::MeshOrchestrator,
};
use nrf_sdc::{self as sdc, mpsl, mpsl::MultiprotocolServiceLayer};
use static_cell::StaticCell;

use crate::{
    adapters::{
        nrf_entropy_adapter::NrfEntropyAdapter, nrf_identity_adapter::NrfIdentityAdapter,
        nrf_nvmc_storage_adapter::NrfNvmcStorageAdapter, nrf_reboot_adapter::NrfRebootAdapter,
        nrf_sleep_adapter::NrfSleepAdapter,
    },
    tasks::{
        battery_task::battery_task,
        ble_task::ble_task,
        lora_task::{LoraGpios, LoraParams, lora_task},
        watchdog_task::watchdog_task,
    },
};

use {defmt_rtt as _, panic_probe as _};

extern crate alloc;

mod adapters;
mod constants;
mod tasks;

/// Heap for core's `alloc` use (protobuf encode/decode, boxed mesh events).
///
/// The ESP32 board gives this 72 KB. Here the budget is tighter: 256 KB total
/// RAM, shared with nrf-sdc/MPSL (which take theirs from the pool handed to
/// them at boot), the BLE packet pool and the LoRa buffers. 32 KB is a starting
/// point — measure real headroom on hardware before trusting it, and see
/// MAX_NODES / DUPLICATE_RING_SIZE in core if it turns out to be too tight.
const HEAP_SIZE: usize = 32 * 1024;

#[global_allocator]
static HEAP: embedded_alloc::LlffHeap = embedded_alloc::LlffHeap::empty();

fn init_heap() {
    use core::mem::MaybeUninit;
    static mut HEAP_MEM: [MaybeUninit<u8>; HEAP_SIZE] = [MaybeUninit::uninit(); HEAP_SIZE];
    // SAFETY: called once, at the very start of main, before anything can
    // allocate; `HEAP_MEM` is not referenced anywhere else.
    unsafe { HEAP.init(&raw mut HEAP_MEM as usize, HEAP_SIZE) }
}

// MPSL owns RADIO, TIMER0, RTC0 and the low-priority EGU/SWI, which is why the
// Embassy time driver is on RTC1 (see Cargo.toml) rather than RTC0.
//
// One binding covers the whole binary — embassy_executor::task fns can't be
// generic, so a task needing an interrupt-bound peripheral (like lora_task's
// SPI) takes this concrete type rather than an `impl Binding<...>` param.
bind_interrupts!(pub struct Irqs {
    RNG => rng::InterruptHandler<RNG>;
    EGU0_SWI0 => mpsl::LowPrioInterruptHandler;
    CLOCK_POWER => mpsl::ClockInterruptHandler;
    RADIO => mpsl::HighPrioInterruptHandler;
    TIMER0 => mpsl::HighPrioInterruptHandler;
    RTC0 => mpsl::HighPrioInterruptHandler;
    SPIM3 => embassy_nrf::spim::InterruptHandler<embassy_nrf::peripherals::SPI3>;
    SAADC => embassy_nrf::saadc::InterruptHandler;
});

#[embassy_executor::task]
async fn mpsl_task(mpsl: &'static MultiprotocolServiceLayer<'static>) -> ! {
    mpsl.run().await
}

/// How many outgoing L2CAP buffers per link.
const L2CAP_TXQ: u8 = 3;
/// How many incoming L2CAP buffers per link.
const L2CAP_RXQ: u8 = 3;
/// Size of L2CAP packets.
const L2CAP_MTU: usize = 251;

fn build_sdc<'d, const N: usize>(
    p: sdc::Peripherals<'d>,
    rng: &'d mut rng::Rng<'d, embassy_nrf::mode::Async>,
    mpsl: &'d MultiprotocolServiceLayer,
    mem: &'d mut sdc::Mem<N>,
) -> Result<sdc::SoftdeviceController<'d>, sdc::Error> {
    sdc::Builder::new()?
        .support_adv()
        .support_peripheral()
        .peripheral_count(1)?
        .buffer_cfg(L2CAP_MTU as u16, L2CAP_MTU as u16, L2CAP_TXQ, L2CAP_RXQ)?
        .build(p, rng, mpsl, mem)
}

#[embassy_executor::main]
async fn main(spawner: Spawner) -> ! {
    init_heap();

    // The XIAO nRF52840 has a 32.768 kHz crystal for the low-frequency clock
    // (upstream's variant.h: `#define USE_LFXO`) — confirmed against the
    // board schematic too. The MPSL lfclk config below tells the controller
    // the same thing.
    let mut nrf_config = embassy_nrf::config::Config::default();
    nrf_config.lfclk_source = embassy_nrf::config::LfclkSource::ExternalXtal;
    let p = embassy_nrf::init(nrf_config);

    // HICHG (P0.13): selects the BQ25101 charger's ISET, driven low for
    // 100 mA instead of the floating-pin/50 mA default. Matches upstream's
    // initVariant(). `main` never returns, so binding this for its scope
    // holds the pin low for the program's entire lifetime.
    let _hichg = Output::new(p.P0_13, Level::Low, OutputDrive::Standard);

    info!("========================================");
    info!("Meshtastenstein - Meshtastic in Rust");
    info!("Target: Seeed XIAO nRF52840 + Wio-SX1262");
    info!("========================================");

    // Node identity from the factory-programmed device ID.
    let identity = NrfIdentityAdapter;
    let mac = match identity.mac_address() {
        Ok(mac) => {
            info!(
                "[Boot] ID: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
                mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
            );
            mac
        }
        Err(e) => {
            info!("[Boot] Failed to derive device ID: {}", e);
            panic!("Cannot continue without a device ID");
        }
    };
    let _node_num = u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]);

    // Start MPSL. This also provides the `critical-section` implementation the
    // rest of the firmware links against, so it has to come up early.
    let mpsl_p =
        mpsl::Peripherals::new(p.RTC0, p.TIMER0, p.TEMP, p.PPI_CH19, p.PPI_CH30, p.PPI_CH31);
    let lfclk_cfg = mpsl::raw::mpsl_clock_lfclk_cfg_t {
        source: mpsl::raw::MPSL_CLOCK_LF_SRC_XTAL as u8,
        // Must be 0 when source is not MPSL_CLOCK_LF_SRC_RC (generated
        // bindings' doc comment on these two fields).
        rc_ctiv: 0,
        rc_temp_ctiv: 0,
        accuracy_ppm: mpsl::raw::MPSL_DEFAULT_CLOCK_ACCURACY_PPM as u16,
        skip_wait_lfclk_started: mpsl::raw::MPSL_DEFAULT_SKIP_WAIT_LFCLK_STARTED != 0,
    };
    static MPSL: StaticCell<MultiprotocolServiceLayer> = StaticCell::new();
    static SESSION_MEM: StaticCell<mpsl::SessionMem<1>> = StaticCell::new();
    let mpsl = MPSL.init(
        mpsl::MultiprotocolServiceLayer::with_timeslots(
            mpsl_p,
            Irqs,
            lfclk_cfg,
            SESSION_MEM.init(mpsl::SessionMem::new()),
        )
        .expect("Failed to initialize MPSL"),
    );
    spawner.spawn(mpsl_task(mpsl).expect("Failed to spawn MPSL task"));
    info!("[Boot] MPSL started");

    // Seed the CSPRNG (and, if needed, a fresh PKC identity) from the
    // hardware TRNG before nrf-sdc borrows the RNG peripheral for the
    // controller's whole lifetime — both draws must happen up front.
    static HW_RNG: StaticCell<rng::Rng<'static, embassy_nrf::mode::Async>> = StaticCell::new();
    let hw_rng = HW_RNG.init(rng::Rng::new(p.RNG, Irqs));
    let mut entropy_seed = [0u8; 32];
    hw_rng.blocking_fill_bytes(&mut entropy_seed);
    let entropy = NrfEntropyAdapter::new(entropy_seed);
    let mut pkc_seed = [0u8; 32];
    hw_rng.blocking_fill_bytes(&mut pkc_seed);
    info!("[Boot] Entropy seeded from hardware TRNG");

    // Bring up the SoftDevice Controller. It implements bt_hci's Controller
    // trait directly, so it's passed straight to trouble_host::new() in
    // ble_task — no ExternalController wrapper needed (that type is for
    // byte-stream HCI transports, which this isn't).
    let sdc_p = sdc::Peripherals::new(
        p.PPI_CH17, p.PPI_CH18, p.PPI_CH20, p.PPI_CH21, p.PPI_CH22, p.PPI_CH23, p.PPI_CH24,
        p.PPI_CH25, p.PPI_CH26, p.PPI_CH27, p.PPI_CH28, p.PPI_CH29,
    );
    static SDC_MEM: StaticCell<sdc::Mem<4096>> = StaticCell::new();
    let sdc = build_sdc(sdc_p, hw_rng, mpsl, SDC_MEM.init(sdc::Mem::new()))
        .expect("Failed to build SoftDevice Controller");
    info!("[Boot] SoftDevice Controller built");

    // NVS storage, backed by mpsl::Flash (writes/erases go through the MPSL
    // timeslot API so they don't collide with radio activity). Must come
    // after MPSL is up, and only once per boot — `Flash::take` panics on a
    // second call. `'static` since the mesh orchestrator holds `&'static mut`
    // to it for the lifetime of the program.
    static STORAGE: StaticCell<NrfNvmcStorageAdapter> = StaticCell::new();
    let flash = mpsl::Flash::take(mpsl, p.NVMC);
    let storage = STORAGE.init(NrfNvmcStorageAdapter::new(flash).await);

    let initial_bond = storage.load_bond().await;

    let mut device = DeviceState::new(&mac);
    storage.load_state(&mut device).await;
    let (lora_modem_cfg, lora_frequency_hz) = device.lora_params();
    info!(
        "[Boot] LoRa params: SF={} BW={}Hz freq={}Hz",
        lora_modem_cfg.spreading_factor, lora_modem_cfg.bandwidth_hz, lora_frequency_hz
    );

    // Load or generate the X25519 PKC keypair. On first boot (or after a
    // factory reset) a fresh 32-byte seed comes from the hardware TRNG so the
    // device keeps the same identity across reboots.
    let pkc_keypair: ([u8; 32], [u8; 32]) = match storage.load_pkc_keypair().await {
        Some(pair) => {
            info!("[Boot] PKC keypair loaded from flash");
            pair
        }
        None => {
            let (secret, public) = keypair_from_seed(pkc_seed);
            let priv_bytes: [u8; 32] = secret.to_bytes();
            let pub_bytes: [u8; 32] = public.to_bytes();
            // A failed save here isn't safe to continue past: every reboot
            // would silently generate a new identity, breaking every peer's
            // ability to decrypt direct messages to this node with no
            // visible symptom beyond "DMs stopped working."
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

    static CHANNELS: StaticCell<Channels> = StaticCell::new();
    let ch = CHANNELS.init(Channels::new());

    let node_num = u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]);
    let lora_gpios = LoraGpios {
        cs: p.P0_04.into(),
        reset: p.P0_28.into(),
        dio1: p.P0_03.into(),
        busy: p.P0_29.into(),
        rxen: p.P0_05.into(),
        sck: p.P1_13,
        miso: p.P1_14,
        mosi: p.P1_15,
    };
    spawner.spawn(
        lora_task(
            p.SPI3,
            lora_gpios,
            ch.lora_tx.receiver(),
            ch.mesh_in.sender(),
            LoraParams {
                node_num,
                modem_cfg: lora_modem_cfg,
                frequency_hz: lora_frequency_hz,
            },
        )
        .expect("Failed to spawn LoRa task"),
    );
    info!("[Boot] Task spawned: LoRa");

    // Build device name: "Meshtastic_XXXX" from last 2 MAC bytes.
    static DEVICE_NAME: StaticCell<heapless::String<24>> = StaticCell::new();
    let device_name: &'static str = {
        let mut name: heapless::String<24> = heapless::String::new();
        name.push_str(BLE_DEVICE_NAME_PREFIX).ok();
        let hex = b"0123456789ABCDEF";
        for &byte in &mac[4..6] {
            name.push(hex[(byte >> 4) as usize] as char).ok();
            name.push(hex[(byte & 0x0f) as usize] as char).ok();
        }
        DEVICE_NAME.init(name).as_str()
    };

    spawner.spawn(
        ble_task(sdc, ch, initial_bond, mac, device_name).expect("Failed to spawn BLE task"),
    );
    info!("[Boot] Task spawned: BLE");

    // Spawn Battery task. Pin assignments and divider values match upstream's
    // seeed_xiao_nrf52840_kit variant.h.
    spawner.spawn(
        battery_task(
            p.SAADC,
            p.P0_31,
            p.P0_14,
            &ch.bat_level,
            ch.mesh_in.sender(),
        )
        .expect("Failed to spawn Battery task"),
    );
    info!("[Boot] Task spawned: Battery");

    // Spawn Watchdog task. `try_new::<WDT, 1>` returns one handle for our
    // single feeder; the hardware timeout (90s) matches upstream's nRF52 port.
    const WATCHDOG_TIMEOUT_MS: u32 = 90_000;
    let mut wdt_config = wdt::Config::default();
    wdt_config.timeout_ticks = WATCHDOG_TIMEOUT_MS / 1000 * 32768;
    // Matches upstream's NRF_WDT_BEHAVIOUR_PAUSE_SLEEP_HALT: the WDT cannot
    // be stopped once started, so if it kept running through System Off,
    // nothing would be left to feed it and the device would reset itself
    // back on ~90s after "shutting down" — defeating the whole point.
    wdt_config.action_during_sleep = wdt::SleepConfig::Pause;
    wdt_config.action_during_debug_halt = wdt::HaltConfig::Pause;
    let (_wdt, [wdt_handle]) = wdt::Watchdog::try_new::<_, 1>(p.WDT, wdt_config)
        .expect("Failed to initialize hardware watchdog");
    spawner.spawn(
        watchdog_task(
            wdt_handle,
            &ch.activity,
            ch.disconn_cmd.sender(),
            NrfSleepAdapter,
            &ch.bat_level,
            &ch.shutdown_cmd,
        )
        .expect("Failed to spawn Watchdog task"),
    );
    info!("[Boot] Task spawned: Watchdog");

    let mut orchestrator =
        MeshOrchestrator::new(ch, &mac, storage, pkc_keypair, NrfRebootAdapter, entropy).await;

    info!("========================================");
    info!("[Boot] BOOT COMPLETE - Starting mesh");
    info!("========================================");

    orchestrator.run().await
}
