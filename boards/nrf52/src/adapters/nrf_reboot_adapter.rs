//! `Reboot` port implementation for nRF52840.

use meshtastenstein_core::ports::Reboot;

// Constructed once the mesh orchestrator is wired up; it takes this as its
// `Reboot` port.
#[allow(dead_code, reason = "wired up with the mesh orchestrator")]
pub struct NrfRebootAdapter;

impl Reboot for NrfRebootAdapter {
    fn reboot(&self) -> ! {
        cortex_m::peripheral::SCB::sys_reset();
    }
}
