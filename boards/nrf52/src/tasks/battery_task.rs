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
use meshtastenstein_core::{constants::OCV_TABLE, inter_task::channels::MeshEvent};

/// R17=1M, R18=510k divider on the XIAO nRF52840 kit's VBAT sense pin.
const ADC_MULTIPLIER: f32 = 3.0;
/// SAADC internal reference (0.6V) x Gain1_6's 1/6 divide = 3.6V full scale.
const ADC_FULL_SCALE_MV: f32 = 3600.0;
const ADC_MAX_COUNT: f32 = 4096.0; // 12-bit resolution

const BATTERY_UPDATE_INTERVAL_SECS: u64 = 60;

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

    let level = read_battery_level(&mut saadc, &mut enable).await;
    info!("[Battery] Initial: {}%", level.0);
    battery_signal.signal(level);
    let _ = mesh_in.try_send(MeshEvent::BatteryUpdate(level.0, level.1));

    loop {
        ticker.next().await;
        let level = read_battery_level(&mut saadc, &mut enable).await;
        debug!("[Battery] {}% ({} mV)", level.0, level.1);
        battery_signal.signal(level);
        let _ = mesh_in.try_send(MeshEvent::BatteryUpdate(level.0, level.1));
    }
}

async fn read_battery_level(
    saadc: &mut Saadc<'static, 1>,
    enable: &mut Output<'static>,
) -> (u8, u16) {
    enable.set_low();
    Timer::after(Duration::from_millis(10)).await;

    let mut buf = [0i16; 1];
    saadc.sample(&mut buf).await;

    enable.set_high();

    let pin_mv = (buf[0].max(0) as f32) * ADC_FULL_SCALE_MV / ADC_MAX_COUNT;
    let scaled_mv = pin_mv * ADC_MULTIPLIER;
    let voltage_mv = scaled_mv as u16;

    (voltage_to_level(voltage_mv), voltage_mv)
}

fn voltage_to_level(mvolts: u16) -> u8 {
    if mvolts >= OCV_TABLE[0] {
        return 100;
    }
    if mvolts <= OCV_TABLE[10] {
        return 0;
    }
    for i in 0..10 {
        if mvolts >= OCV_TABLE[i + 1] {
            let v_high = OCV_TABLE[i] as u32;
            let v_low = OCV_TABLE[i + 1] as u32;
            let v = mvolts as u32;
            let pct_high = (100 - i * 10) as u32;
            let pct_low = (100 - (i + 1) * 10) as u32;
            return (pct_low + (v - v_low) * (pct_high - pct_low) / (v_high - v_low)) as u8;
        }
    }
    0
}
