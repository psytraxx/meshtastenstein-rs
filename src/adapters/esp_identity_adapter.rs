//! Adapter for ESP32-specific device identity.

use crate::ports::Identity;
use esp_hal::efuse::Efuse;

/// Identity adapter using ESP32 eFuse.
pub struct EspIdentityAdapter;

impl Identity for EspIdentityAdapter {
    fn mac_address(&self) -> [u8; 6] {
        Efuse::read_base_mac_address()
    }
}
