//! Builds the NodeinfoApp payload (a serialised `User` proto).

use crate::{domain::device::DeviceState, proto::User};
use prost::Message;

#[allow(deprecated)] // User::macaddr is deprecated in proto but still sent on-wire
pub fn build_payload(
    device: &DeviceState,
    node_id_str: &str,
    pkc_pub: &[u8; 32],
) -> alloc::vec::Vec<u8> {
    // Only include the public key if it's non-zero (i.e. the keypair has been
    // generated). All-zero means "not yet initialised" (should never happen at
    // build time, but be defensive).
    let public_key = if pkc_pub.iter().any(|&b| b != 0) {
        pkc_pub.to_vec()
    } else {
        alloc::vec::Vec::new()
    };

    User {
        id: node_id_str.into(),
        long_name: device.long_name.as_str().into(),
        short_name: device.short_name.as_str().into(),
        macaddr: device.mac.to_vec(),
        hw_model: device.hw_model as i32,
        role: device.role as i32,
        public_key,
        ..Default::default()
    }
    .encode_to_vec()
}
