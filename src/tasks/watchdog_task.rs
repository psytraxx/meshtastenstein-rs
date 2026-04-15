//! Watchdog task - feeds HW WDT, monitors inactivity, triggers deep sleep

use crate::{
    adapters::deep_sleep_adapter::DeepSleepAdapter,
    constants::{INACTIVITY_TIMEOUT_MS, LOW_BATTERY_THRESHOLD},
    ports::Sleep,
};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Sender, signal::Signal};
use embassy_time::{Duration, Instant, Timer, WithTimeout};
use esp_hal::{peripherals::TIMG1, timer::timg::Wdt};
use log::{info, warn};

const WATCHDOG_FEED_INTERVAL_MS: u64 = 500;
/// Grace period after BLE disconnect before entering deep sleep
const SLEEP_GRACE_MS: u64 = 500;

#[embassy_executor::task]
pub async fn watchdog_task(
    mut wdt: Wdt<TIMG1<'static>>,
    activity_signal: &'static Signal<CriticalSectionRawMutex, Instant>,
    disconnect_sender: Sender<'static, CriticalSectionRawMutex, (), 1>,
    sleep: &'static mut DeepSleepAdapter<'static>,
    bat_level: &'static Signal<CriticalSectionRawMutex, (u8, u16)>,
    shutdown_cmd: &'static Signal<CriticalSectionRawMutex, u32>,
) {
    info!(
        "[Watchdog] Starting (feed={}ms, inactivity={}ms)",
        WATCHDOG_FEED_INTERVAL_MS, INACTIVITY_TIMEOUT_MS
    );

    let timeout_duration = Duration::from_millis(INACTIVITY_TIMEOUT_MS);
    let feed_interval = Duration::from_millis(WATCHDOG_FEED_INTERVAL_MS);
    let mut last_activity = Instant::now();

    loop {
        wdt.feed();

        if let Ok(activity_time) = activity_signal.wait().with_timeout(feed_interval).await {
            last_activity = activity_time;
        }

        // Admin-requested shutdown (highest priority — user explicitly asked)
        if let Some(secs) = shutdown_cmd.try_take() {
            warn!(
                "[Watchdog] Shutdown requested in {}s — disconnecting BLE then sleeping",
                secs
            );
            let _ = disconnect_sender.try_send(());
            Timer::after(Duration::from_millis(SLEEP_GRACE_MS)).await;
            if secs > 0 {
                Timer::after(Duration::from_secs(secs as u64)).await;
            }
            sleep.enter_sleep();
        }

        // Low battery auto-sleep check
        if let Some((level, _voltage_mv)) = bat_level.try_take()
            && level > 0
            && level <= LOW_BATTERY_THRESHOLD
        {
            warn!(
                "[Watchdog] Low battery ({}%) — disconnecting BLE then sleeping",
                level
            );
            let _ = disconnect_sender.try_send(());
            Timer::after(Duration::from_millis(SLEEP_GRACE_MS)).await;
            sleep.enter_sleep(); // resets CPU — does not return
        }

        let elapsed = Instant::now().duration_since(last_activity);
        if elapsed >= timeout_duration {
            warn!(
                "[Watchdog] Inactivity timeout ({}s) — disconnecting BLE then sleeping",
                elapsed.as_secs()
            );
            let _ = disconnect_sender.try_send(());
            // Give BLE stack time to close the connection cleanly
            Timer::after(Duration::from_millis(SLEEP_GRACE_MS)).await;
            sleep.enter_sleep(); // resets CPU — does not return
        }
    }
}
