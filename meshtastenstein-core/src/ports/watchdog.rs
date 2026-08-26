//! Port (interface) for hardware watchdog feeding.

/// Port trait for feeding a hardware watchdog timer.
///
/// Deliberately minimal — `embedded-hal` dropped its watchdog trait in 1.0,
/// and neither board needs more than "reset the countdown." Configuration
/// (timeout, sleep/halt behavior) stays board-specific since it happens once
/// at boot alongside other peripheral setup, not on the task's hot path.
pub trait Watchdog {
    fn feed(&mut self);
}
