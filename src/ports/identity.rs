//! Port (interface) for device identity.

/// Port trait for providing device identity (e.g. MAC address).
pub trait Identity {
    /// Get the 6-byte hardware MAC address.
    fn mac_address(&self) -> [u8; 6];

    /// Get the 4-byte node number derived from the MAC address.
    fn node_num(&self) -> u32 {
        let mac = self.mac_address();
        u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]])
    }
}
