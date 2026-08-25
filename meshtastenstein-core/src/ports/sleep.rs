//! Port (interface) for sleep/power management.

/// Port trait for sleep/power management operations
pub trait Sleep {
    /// Enter deep sleep indefinitely until GPIO wakeup (LoRa DIO1 or button press).
    /// Deep sleep resets the CPU - this function does not return.
    fn enter_sleep(&mut self) -> !;
}
