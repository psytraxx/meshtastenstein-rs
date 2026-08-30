//! Battery monitoring task — reads VBAT over SAADC and publishes level updates.
//!
//! Pin assignments and divider values come from upstream Meshtastic's own
//! `seeed_xiao_nrf52840_kit` variant.h: `PIN_VBAT` (P0.31) through a
//! 1M/510k divider (`ADC_MULTIPLIER = 3`), gated by `VBAT_ENABLE` (P0.14,
//! active low — driving it low connects the divider).

use embassy_nrf::{
    Peri,
    gpio::{Level, Output, OutputDrive},
    peripherals::{P0_14, P0_31, SAADC},
    saadc::{self, ChannelConfig, Saadc},
};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Sender, signal::Signal};
use embassy_time::{Duration, Ticker, Timer};
use log::{debug, info};
use meshtastenstein_core::{domain::battery::voltage_to_level, inter_task::channels::MeshEvent};

/// R17=1M, R18=510k divider on the XIAO nRF52840 kit's VBAT sense pin.
const ADC_MULTIPLIER: f32 = 3.0;
/// SAADC internal reference (0.6V) x Gain1_6's 1/6 divide = 3.6V full scale.
const ADC_FULL_SCALE_MV: f32 = 3600.0;
const ADC_MAX_COUNT: f32 = 4096.0; // 12-bit resolution

const BATTERY_UPDATE_INTERVAL_SECS: u64 = 60;

/// Averaging samples per reading and the smoothing coefficient below both
/// match the ESP32 board's battery task, which in turn matches upstream's
/// own `BATTERY_SENSE_SAMPLES`/virtual-LPF approach — needed here for the
/// same reason: a single unfiltered sample taken during a LoRa TX burst can
/// read low enough to trip the watchdog task's automatic low-battery
/// shutdown on noise alone.
const BATTERY_SENSE_SAMPLES: u32 = 15;

#[embassy_executor::task]
pub async fn battery_task(
    saadc_p: Peri<'static, SAADC>,
    vbat_pin: Peri<'static, P0_31>,
    vbat_enable_pin: Peri<'static, P0_14>,
    battery_signal: &'static Signal<CriticalSectionRawMutex, (u8, u16)>,
    mesh_in: Sender<'static, CriticalSectionRawMutex, MeshEvent, 8>,
) {
    info!("[Battery] Starting battery monitoring task");

    // Active low: driving it low connects the VBAT divider to the ADC pin.
    let mut enable = Output::new(vbat_enable_pin, Level::High, OutputDrive::Standard);

    let channel_config = ChannelConfig::single_ended(vbat_pin);
    let mut saadc = Saadc::new(
        saadc_p,
        crate::Irqs,
        saadc::Config::default(),
        [channel_config],
    );
    saadc.calibrate().await;

    let mut ticker = Ticker::every(Duration::from_secs(BATTERY_UPDATE_INTERVAL_SECS));
    let mut last_voltage: f32 = 3700.0;
    let mut initial_read_done = false;

    let level = read_battery_level(
        &mut saadc,
        &mut enable,
        &mut last_voltage,
        &mut initial_read_done,
    )
    .await;
    info!("[Battery] Initial: {}% ({:.0} mV)", level.0, last_voltage);
    battery_signal.signal(level);
    let _ = mesh_in.try_send(MeshEvent::BatteryUpdate(level.0, level.1));

    loop {
        ticker.next().await;
        let level = read_battery_level(
            &mut saadc,
            &mut enable,
            &mut last_voltage,
            &mut initial_read_done,
        )
        .await;
        debug!("[Battery] {}% ({} mV)", level.0, level.1);
        battery_signal.signal(level);
        let _ = mesh_in.try_send(MeshEvent::BatteryUpdate(level.0, level.1));
    }
}

async fn read_battery_level(
    saadc: &mut Saadc<'static, 1>,
    enable: &mut Output<'static>,
    last_voltage: &mut f32,
    initial_read_done: &mut bool,
) -> (u8, u16) {
    enable.set_low();
    Timer::after(Duration::from_millis(10)).await;

    let mut raw_sum: i32 = 0;
    let mut valid_samples: u32 = 0;
    let mut buf = [0i16; 1];
    for _ in 0..BATTERY_SENSE_SAMPLES {
        saadc.sample(&mut buf).await;
        raw_sum += buf[0].max(0) as i32;
        valid_samples += 1;
        embassy_futures::yield_now().await;
    }

    enable.set_high();

    let raw_avg = if valid_samples > 0 {
        raw_sum as f32 / valid_samples as f32
    } else {
        0.0
    };
    let pin_mv = raw_avg * ADC_FULL_SCALE_MV / ADC_MAX_COUNT;
    let scaled_mv = pin_mv * ADC_MULTIPLIER;

    if !*initial_read_done {
        if scaled_mv > *last_voltage {
            *last_voltage = scaled_mv;
        }
        *initial_read_done = true;
    } else {
        *last_voltage += (scaled_mv - *last_voltage) * 0.5;
    }

    let voltage_mv = *last_voltage as u16;
    (voltage_to_level(voltage_mv), voltage_mv)
}
