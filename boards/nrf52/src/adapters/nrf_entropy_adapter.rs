//! `EntropySource` port implementation for nRF52840.
//!
//! The nRF52840's RNG peripheral is handed to nrf-sdc at boot and borrowed
//! mutably for the controller's entire lifetime, so we cannot read it directly
//! — doing so would race with the controller's own use of it.
//!
//! Instead this is a ChaCha20 CSPRNG seeded once at boot, before the RNG is
//! given away. `EntropySource::random_u32` takes `&self`, so the stream state
//! sits behind a critical-section mutex.
//!
//! This backs rebroadcast jitter and — security-relevant — the nonce on PKC
//! encrypted direct messages. A CSPRNG seeded from the hardware TRNG is
//! appropriate for both; what would not be acceptable is a fixed seed, since
//! repeating a nonce across reboots would break the direct-message encryption.

use core::cell::RefCell;

use critical_section::Mutex;
use meshtastenstein_core::ports::EntropySource;
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};

pub struct NrfEntropyAdapter {
    rng: Mutex<RefCell<ChaCha20Rng>>,
}

impl NrfEntropyAdapter {
    /// Seed from the hardware TRNG. Call at boot, before the RNG peripheral is
    /// handed to nrf-sdc.
    pub fn new(seed: [u8; 32]) -> Self {
        Self {
            rng: Mutex::new(RefCell::new(ChaCha20Rng::from_seed(seed))),
        }
    }
}

impl EntropySource for NrfEntropyAdapter {
    fn random_u32(&self) -> u32 {
        critical_section::with(|cs| self.rng.borrow_ref_mut(cs).next_u32())
    }
}
