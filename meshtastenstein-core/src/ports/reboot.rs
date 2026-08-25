//! Port (interface) for triggering a CPU reset.

/// Port trait for performing a software reset/reboot.
pub trait Reboot {
    /// Reset the CPU. Does not return.
    fn reboot(&self) -> !;
}
