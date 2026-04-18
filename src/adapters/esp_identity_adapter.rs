//! Adapter for ESP32-specific device identity.

use crate::ports::Identity;

/// Identity adapter using ESP32 eFuse.
pub struct EspIdentityAdapter;

impl Identity for EspIdentityAdapter {
    fn mac_address(&self) -> Result<[u8; 6], &'static str> {
        esp_hal::efuse::base_mac_address()
            .as_bytes()
            .try_into()
            .map_err(|_| "Failed to convert MAC address to [u8; 6]")
    }
}
