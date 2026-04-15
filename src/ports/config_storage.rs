//! Port (interface) for device configuration and bond persistence.

use crate::domain::{device::DeviceState, node_db::NodeDB};

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

    /// Erase all persisted device configuration (factory reset).
    fn erase_config(&mut self);

    /// Persist the current `NodeDB` snapshot (most-recently-heard peers).
    /// Idempotent — callers should gate on `NodeDB::is_dirty()` to avoid
    /// excess flash wear.
    fn save_node_db(&mut self, db: &NodeDB);

    /// Load a previously persisted `NodeDB` snapshot into `db`. No-op if no
    /// snapshot exists or if the on-disk version is unrecognized.
    fn load_node_db(&mut self, db: &mut NodeDB);

    /// Load the persistent X25519 keypair `(priv, pub)`. Returns `None` when
    /// the device hasn't generated one yet (first boot or v2 → v3 upgrade).
    fn load_pkc_keypair(&mut self) -> Option<([u8; 32], [u8; 32])>;

    /// Persist a freshly-generated X25519 keypair. Replaces any previous
    /// keypair — callers must be careful to only do this once per device.
    fn save_pkc_keypair(&mut self, priv_key: &[u8; 32], pub_key: &[u8; 32]);
}
