//! `Reboot` port implementation for ESP32-S3.

use meshtastenstein_core::ports::Reboot;

pub struct EspRebootAdapter;

impl Reboot for EspRebootAdapter {
    fn reboot(&self) -> ! {
        esp_hal::system::software_reset();
    }
}
