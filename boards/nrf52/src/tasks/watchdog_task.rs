//! Watchdog task — thin board wrapper. The feed/shutdown logic lives once in
//! `meshtastenstein_core::tasks::watchdog_task_body`, shared with the ESP32
//! board; this file only supplies the concrete `wdt::WatchdogHandle` type
//! (as `ports::Watchdog`), `NrfSleepAdapter` (already `ports::Sleep`), and
//! opts out of inactivity-triggered sleep — see the `false` argument below
//! and `watchdog_task_body`'s doc comment for why.

use crate::adapters::nrf_sleep_adapter::NrfSleepAdapter;
use embassy_nrf::wdt;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Sender, signal::Signal};
use embassy_time::Instant;
use meshtastenstein_core::{ports, tasks::watchdog_task_body};

struct NrfWatchdog(wdt::WatchdogHandle);

impl ports::Watchdog for NrfWatchdog {
    fn feed(&mut self) {
        self.0.pet();
    }
}

#[embassy_executor::task]
pub async fn watchdog_task(
    handle: wdt::WatchdogHandle,
    activity_signal: &'static Signal<CriticalSectionRawMutex, Instant>,
    disconnect_sender: Sender<'static, CriticalSectionRawMutex, (), 1>,
    sleep: NrfSleepAdapter,
    bat_level: &'static Signal<CriticalSectionRawMutex, (u8, u16)>,
    shutdown_cmd: &'static Signal<CriticalSectionRawMutex, u32>,
) {
    watchdog_task_body::run(
        NrfWatchdog(handle),
        sleep,
        activity_signal,
        disconnect_sender,
        bat_level,
        shutdown_cmd,
        // System Off has no wake source on this board, so sleeping on mesh
        // inactivity would permanently drop a healthy node off the mesh
        // until someone physically resets it — worse than staying awake
        // with the radio duty-cycled. Admin shutdown and low-battery
        // shutdown still apply below; only this trigger is board-specific.
        false,
    )
    .await
}
