//! `EntropySource` port implementation for ESP32-S3.

use esp_hal::rng::Rng;
use meshtastenstein_core::ports::EntropySource;

pub struct EspEntropyAdapter;

impl EntropySource for EspEntropyAdapter {
    fn random_u32(&self) -> u32 {
        Rng::new().random()
    }
}
