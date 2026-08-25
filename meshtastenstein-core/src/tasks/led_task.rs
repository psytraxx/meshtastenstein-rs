//! LED indicator task

use crate::{
    constants::{LED_BLINK_DELAY_MS, LED_HEARTBEAT_ON_MS, LED_ON_MS},
    inter_task::channels::{LedCommand, LedPattern},
};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Receiver};
use embassy_time::{Duration, Timer};
use embedded_hal::digital::OutputPin;
use log::info;

/// LED task body. `embassy_executor::task` functions can't be generic, so
/// each board wraps this in its own concrete `#[embassy_executor::task]` fn.
pub async fn led_task<P: OutputPin + 'static>(
    mut led_pin: P,
    receiver: Receiver<'static, CriticalSectionRawMutex, LedCommand, 5>,
) {
    info!("[LED] Starting LED task");

    loop {
        let cmd = receiver.receive().await;
        match cmd {
            LedCommand::Blink(pattern) => {
                execute_pattern(&mut led_pin, pattern).await;
            }
        }
    }
}

async fn execute_pattern<P: OutputPin>(led_pin: &mut P, pattern: LedPattern) {
    match pattern {
        LedPattern::SingleBlink => {
            single_blink(led_pin).await;
        }
        LedPattern::DoubleBlink => {
            single_blink(led_pin).await;
            single_blink(led_pin).await;
        }
        LedPattern::Heartbeat => {
            let _ = led_pin.set_high();
            Timer::after(Duration::from_millis(LED_HEARTBEAT_ON_MS)).await;
            let _ = led_pin.set_low();
        }
    }
}

async fn single_blink<P: OutputPin>(led_pin: &mut P) {
    let _ = led_pin.set_high();
    Timer::after(Duration::from_millis(LED_ON_MS)).await;
    let _ = led_pin.set_low();
    Timer::after(Duration::from_millis(LED_BLINK_DELAY_MS)).await;
}
