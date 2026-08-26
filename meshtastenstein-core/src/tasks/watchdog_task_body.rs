//! Watchdog task body: feeds the hardware watchdog, monitors inactivity, and
//! triggers sleep on an admin-requested shutdown, low battery, or (on boards
//! that opt in) inactivity timeout. `embassy_executor::task` functions can't
//! be generic, so each board wraps this in its own concrete
//! `#[embassy_executor::task]` fn — same pattern as `led_task.rs`.
//!
//! Generic over two board-supplied ports: `W: Watchdog` for the feed
//! mechanism (`embedded-hal` has no watchdog trait since 1.0, hence this
//! crate's own minimal one) and `S: Sleep` for what "sleep" means on that
//! board — deep sleep with wake-on-LoRa on the ESP32 board, System Off with
//! no wake source at all on the nRF52 board.
//!
//! `sleep_on_inactivity` exists because of that last point: sleep here isn't
//! symmetric across boards. On ESP32, an idle node powering down and waking
//! back up on the next LoRa packet is a genuine power saving with no
//! downside — the node keeps participating in the mesh either way. On
//! nRF52, `enter_sleep()` has no wake source, so the same trigger would
//! permanently drop a healthy node off the mesh until someone physically
//! resets it — the opposite of what an idle relay node should do, and worse
//! than just staying awake with the radio duty-cycled. Admin-requested
//! shutdown and low-battery shutdown are both deliberate "go dark" decisions
//! (a person asked, or the battery is nearly dead) and apply on every board
//! regardless.

use crate::{
    constants::{INACTIVITY_TIMEOUT_MS, LOW_BATTERY_THRESHOLD},
    ports::{Sleep, Watchdog},
};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Sender, signal::Signal};
use embassy_time::{Duration, Instant, Timer, WithTimeout};
use log::{info, warn};

const WATCHDOG_FEED_INTERVAL_MS: u64 = 500;
/// Grace period after BLE disconnect before entering sleep.
const SLEEP_GRACE_MS: u64 = 500;

#[allow(clippy::too_many_arguments)]
pub async fn run<W: Watchdog, S: Sleep>(
    mut wdt: W,
    mut sleep: S,
    activity_signal: &'static Signal<CriticalSectionRawMutex, Instant>,
    disconnect_sender: Sender<'static, CriticalSectionRawMutex, (), 1>,
    bat_level: &'static Signal<CriticalSectionRawMutex, (u8, u16)>,
    shutdown_cmd: &'static Signal<CriticalSectionRawMutex, u32>,
    sleep_on_inactivity: bool,
) -> ! {
    info!(
        "[Watchdog] Starting (feed={}ms, inactivity={}ms, sleep_on_inactivity={})",
        WATCHDOG_FEED_INTERVAL_MS, INACTIVITY_TIMEOUT_MS, sleep_on_inactivity
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
            // Feed the watchdog while waiting out the requested delay —
            // `secs` can exceed the WDT timeout, and a long unfed sleep here
            // would reset the device instead of letting it shut down.
            let mut remaining_secs = secs as u64;
            while remaining_secs > 0 {
                let chunk = remaining_secs.min(feed_interval.as_secs().max(1));
                Timer::after(Duration::from_secs(chunk)).await;
                wdt.feed();
                remaining_secs -= chunk;
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
            sleep.enter_sleep();
        }

        if sleep_on_inactivity {
            let elapsed = Instant::now().duration_since(last_activity);
            if elapsed >= timeout_duration {
                warn!(
                    "[Watchdog] Inactivity timeout ({}s) — disconnecting BLE then sleeping",
                    elapsed.as_secs()
                );
                let _ = disconnect_sender.try_send(());
                // Give BLE stack time to close the connection cleanly
                Timer::after(Duration::from_millis(SLEEP_GRACE_MS)).await;
                sleep.enter_sleep();
            }
        }
    }
}
