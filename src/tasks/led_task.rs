//! LED indicator task

use crate::{
    constants::{LED_BLINK_DELAY_MS, LED_HEARTBEAT_ON_MS, LED_ON_MS},
    inter_task::channels::{LedCommand, LedPattern},
};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Receiver};
use embassy_time::{Duration, Timer};
use esp_hal::gpio::{AnyPin, Level, Output, OutputConfig};
use log::info;

#[embassy_executor::task]
pub async fn led_task(
    pin: AnyPin<'static>,
    receiver: Receiver<'static, CriticalSectionRawMutex, LedCommand, 5>,
) {
    info!("[LED] Starting LED task");
    let mut led_pin = Output::new(pin, Level::Low, OutputConfig::default());

    loop {
        let cmd = receiver.receive().await;
        match cmd {
            LedCommand::Blink(pattern) => {
                execute_pattern(&mut led_pin, pattern).await;
            }
        }
    }
}

async fn execute_pattern(led_pin: &mut Output<'static>, pattern: LedPattern) {
    match pattern {
        LedPattern::SingleBlink => {
            single_blink(led_pin).await;
        }
        LedPattern::DoubleBlink => {
            single_blink(led_pin).await;
            single_blink(led_pin).await;
        }
        LedPattern::Heartbeat => {
            led_pin.set_high();
            Timer::after(Duration::from_millis(LED_HEARTBEAT_ON_MS)).await;
            led_pin.set_low();
        }
    }
}

async fn single_blink(led_pin: &mut Output<'static>) {
    led_pin.set_high();
    Timer::after(Duration::from_millis(LED_ON_MS)).await;
    led_pin.set_low();
    Timer::after(Duration::from_millis(LED_BLINK_DELAY_MS)).await;
}
