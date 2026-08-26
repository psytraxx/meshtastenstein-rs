//! `Reboot` port implementation for nRF52840.

use meshtastenstein_core::ports::Reboot;

pub struct NrfRebootAdapter;

impl Reboot for NrfRebootAdapter {
    fn reboot(&self) -> ! {
        cortex_m::peripheral::SCB::sys_reset();
    }
}
