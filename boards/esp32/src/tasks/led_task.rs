//! ESP32-S3 wrapper spawning the shared LED task body.

use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Receiver};
use esp_hal::gpio::Output;
use meshtastenstein_core::{inter_task::channels::LedCommand, tasks::led_task::led_task};

#[embassy_executor::task]
pub async fn esp_led_task(
    pin: Output<'static>,
    receiver: Receiver<'static, CriticalSectionRawMutex, LedCommand, 5>,
) {
    led_task(pin, receiver).await
}
