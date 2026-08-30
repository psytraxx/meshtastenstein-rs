//! Port (interface) for sleep/power management.

/// Port trait for sleep/power management operations
pub trait Sleep {
    /// Enter deep sleep indefinitely until GPIO wakeup (LoRa DIO1 or button press).
    /// Deep sleep resets the CPU - this function does not return.
    fn enter_sleep(&mut self) -> !;
}

/// Blanket impl so a `&'static mut S` (how boards that construct their
/// `Sleep` adapter in a `StaticCell` naturally hold it) can be passed
/// directly wherever an owned `S: Sleep` is expected, e.g.
/// `tasks::watchdog_task_body::run`.
impl<S: Sleep + ?Sized> Sleep for &mut S {
    fn enter_sleep(&mut self) -> ! {
        (**self).enter_sleep()
    }
}
