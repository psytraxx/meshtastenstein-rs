//! Port (interface) for device configuration and bond persistence.

use crate::domain::device::DeviceState;

/// Port trait for persisting device configuration and BLE bond data.
///
/// The adapter (e.g. `NvsStorageAdapter`) is responsible for all serialization;
/// callers work purely in domain types.
pub trait ConfigStorage {
    /// Persist the current device state to non-volatile storage.
    fn save_state(&mut self, device: &DeviceState);

    /// Load a previously persisted device state into `device`.
    /// No-op if no saved state exists (first boot or corrupted flash).
    fn load_state(&mut self, device: &mut DeviceState);

    /// Persist a raw 48-byte BLE bond blob.
    fn save_bond(&mut self, bytes: &[u8; 48]);

    /// Load the raw 48-byte BLE bond blob, or `None` if absent/corrupt.
    fn load_bond(&mut self) -> Option<[u8; 48]>;

    /// Erase the stored bond (e.g. on factory reset).
    fn clear_bond(&mut self);
}
