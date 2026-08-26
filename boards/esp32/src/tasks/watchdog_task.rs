//! Watchdog task — thin board wrapper. The feed/inactivity/shutdown logic
//! lives once in `meshtastenstein_core::tasks::watchdog_task_body`, shared
//! with the nRF52 board; this file only supplies the concrete `Wdt` type
//! (as `ports::Watchdog`) and DeepSleepAdapter (already `ports::Sleep`).

use crate::adapters::deep_sleep_adapter::DeepSleepAdapter;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Sender, signal::Signal};
use embassy_time::Instant;
use esp_hal::{peripherals::TIMG1, timer::timg::Wdt};
use meshtastenstein_core::{ports, tasks::watchdog_task_body};

struct EspWatchdog(Wdt<TIMG1<'static>>);

impl ports::Watchdog for EspWatchdog {
    fn feed(&mut self) {
        self.0.feed();
    }
}

#[embassy_executor::task]
pub async fn watchdog_task(
    wdt: Wdt<TIMG1<'static>>,
    activity_signal: &'static Signal<CriticalSectionRawMutex, Instant>,
    disconnect_sender: Sender<'static, CriticalSectionRawMutex, (), 1>,
    sleep: &'static mut DeepSleepAdapter<'static>,
    bat_level: &'static Signal<CriticalSectionRawMutex, (u8, u16)>,
    shutdown_cmd: &'static Signal<CriticalSectionRawMutex, u32>,
) {
    watchdog_task_body::run(
        EspWatchdog(wdt),
        sleep,
        activity_signal,
        disconnect_sender,
        bat_level,
        shutdown_cmd,
        true, // deep sleep wakes on the next LoRa packet (DIO1/EXT0) — no downside to sleeping
    )
    .await
}
