//! Battery monitoring task - reads ADC and sends level updates

use crate::constants::OCV_TABLE;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Sender;
use embassy_time::{Duration, Ticker, Timer};
use esp_hal::Blocking;
use esp_hal::analog::adc::{Adc, AdcPin};
use esp_hal::gpio::{AnyPin, Flex, InputConfig, Pull};
use esp_hal::peripherals::{ADC1, GPIO1};
use log::{debug, error, info, warn};

const BATTERY_SENSE_SAMPLES: u32 = 15;
const BATTERY_UPDATE_INTERVAL_SECS: u64 = 60;

#[embassy_executor::task]
pub async fn battery_task(
    mut adc: Adc<'static, ADC1<'static>, Blocking>,
    mut pin: AdcPin<GPIO1<'static>, ADC1<'static>>,
    divider_ratio: f32,
    ctrl_pin: Option<AnyPin<'static>>,
    battery_sender: Sender<'static, CriticalSectionRawMutex, u8, 1>,
) {
    info!("[Battery] Starting battery monitoring task");

    let mut ctrl_pin: Option<Flex<'static>> = ctrl_pin.map(|p| {
        let mut flex = Flex::new(p);
        flex.apply_input_config(&InputConfig::default().with_pull(Pull::Down));
        flex.set_output_enable(false);
        flex.set_input_enable(true);
        flex
    });

    let mut last_voltage: f32 = 3700.0;
    let mut initial_read_done = false;
    let mut ticker = Ticker::every(Duration::from_secs(BATTERY_UPDATE_INTERVAL_SECS));

    let level = read_battery_level(
        &mut adc, &mut pin, divider_ratio, &mut ctrl_pin, &mut last_voltage, &mut initial_read_done,
    ).await;
    info!("[Battery] Initial: {}% ({:.0} mV)", level, last_voltage);
    let _ = battery_sender.try_send(level);

    loop {
        ticker.next().await;
        let level = read_battery_level(
            &mut adc, &mut pin, divider_ratio, &mut ctrl_pin, &mut last_voltage, &mut initial_read_done,
        ).await;
        debug!("[Battery] {}% ({:.0} mV)", level, last_voltage);
        let _ = battery_sender.try_send(level);
    }
}

async fn read_battery_level(
    adc: &mut Adc<'static, ADC1<'static>, Blocking>,
    pin: &mut AdcPin<GPIO1<'static>, ADC1<'static>>,
    divider_ratio: f32,
    ctrl_pin: &mut Option<Flex<'static>>,
    last_voltage: &mut f32,
    initial_read_done: &mut bool,
) -> u8 {
    // Enable ADC circuit
    if let Some(ctrl) = ctrl_pin.as_mut() {
        ctrl.set_low();
        ctrl.set_input_enable(false);
        ctrl.set_output_enable(true);
    }

    Timer::after(Duration::from_millis(10)).await;

    let mut raw_sum: u32 = 0;
    let mut valid_samples: u32 = 0;

    for _ in 0..BATTERY_SENSE_SAMPLES {
        match nb::block!(adc.read_oneshot(pin)) {
            Ok(raw) => {
                raw_sum += raw as u32;
                valid_samples += 1;
            }
            Err(_) => warn!("[Battery] ADC read error"),
        }
        embassy_futures::yield_now().await;
    }

    // Disable ADC circuit
    if let Some(ctrl) = ctrl_pin.as_mut() {
        ctrl.set_output_enable(false);
        ctrl.apply_input_config(&InputConfig::default().with_pull(Pull::Down));
        ctrl.set_input_enable(true);
    }

    if valid_samples == 0 {
        error!("[Battery] No valid samples!");
        return voltage_to_level(*last_voltage as u16);
    }

    let raw_avg = raw_sum / valid_samples;
    let pin_mv = (raw_avg * 2450 / 4095) as u16;
    let scaled_mv = pin_mv as f32 * divider_ratio;

    if !*initial_read_done {
        if scaled_mv > *last_voltage {
            *last_voltage = scaled_mv;
        }
        *initial_read_done = true;
    } else {
        *last_voltage += (scaled_mv - *last_voltage) * 0.5;
    }

    voltage_to_level(*last_voltage as u16)
}

fn voltage_to_level(mvolts: u16) -> u8 {
    if mvolts >= OCV_TABLE[0] { return 100; }
    if mvolts <= OCV_TABLE[10] { return 0; }
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
