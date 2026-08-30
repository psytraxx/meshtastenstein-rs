//! Adapter for nRF52840-specific device identity.

use embassy_nrf::pac;
use meshtastenstein_core::ports::Identity;

/// Identity adapter using the nRF52840's factory-programmed device ID.
///
/// The nRF52840 has no MAC address in the Ethernet sense; FICR holds a 64-bit
/// DEVICEID fixed at manufacture. We take the low 6 bytes and set the two most
/// significant bits of the first byte, which marks the result as a
/// locally-administered unicast address — the same convention Nordic's own
/// stack uses when deriving a BLE random static address from DEVICEID.
///
/// The node number the mesh uses is derived from the last 4 bytes, so it stays
/// stable across reboots and reflashes.
pub struct NrfIdentityAdapter;

impl Identity for NrfIdentityAdapter {
    fn mac_address(&self) -> Result<[u8; 6], &'static str> {
        let ficr = pac::FICR;
        let low = ficr.deviceid(0).read();
        let high = ficr.deviceid(1).read();
        let id = (u64::from(high) << 32) | u64::from(low);

        let mut mac = [0u8; 6];
        mac.copy_from_slice(&id.to_le_bytes()[..6]);
        // Locally administered, unicast.
        mac[0] = (mac[0] | 0b1100_0000) & 0b1111_1110;
        Ok(mac)
    }
}
