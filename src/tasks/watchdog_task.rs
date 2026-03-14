//! Watchdog task - feeds HW WDT and monitors inactivity

use crate::constants::INACTIVITY_TIMEOUT_MS;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Sender;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, WithTimeout};
use esp_hal::peripherals::TIMG1;
use esp_hal::timer::timg::Wdt;
use log::{info, warn};

const WATCHDOG_FEED_INTERVAL_MS: u64 = 500;

#[embassy_executor::task]
pub async fn watchdog_task(
    mut wdt: Wdt<TIMG1<'static>>,
    activity_signal: &'static Signal<CriticalSectionRawMutex, Instant>,
    disconnect_sender: Sender<'static, CriticalSectionRawMutex, (), 1>,
) {
    info!("[Watchdog] Starting (feed={}ms, inactivity={}ms)", WATCHDOG_FEED_INTERVAL_MS, INACTIVITY_TIMEOUT_MS);

    let timeout_duration = Duration::from_millis(INACTIVITY_TIMEOUT_MS);
    let feed_interval = Duration::from_millis(WATCHDOG_FEED_INTERVAL_MS);
    let mut last_activity = Instant::now();

    loop {
        wdt.feed();

        if let Ok(activity_time) = activity_signal.wait().with_timeout(feed_interval).await {
            last_activity = activity_time;
        }

        let elapsed = Instant::now().duration_since(last_activity);
        if elapsed >= timeout_duration {
            warn!("[Watchdog] Inactivity timeout: {}s", elapsed.as_secs());
            let _ = disconnect_sender.try_send(());
            last_activity = Instant::now();
        }
    }
}
