//! Builds the NodeinfoApp payload (a serialised `User` proto).

use crate::domain::device::DeviceState;
use crate::proto::User;
use prost::Message;

#[allow(deprecated)] // User::macaddr is deprecated in proto but still sent on-wire
pub fn build_payload(device: &DeviceState, node_id_str: &str) -> alloc::vec::Vec<u8> {
    User {
        id: node_id_str.into(),
        long_name: device.long_name.as_str().into(),
        short_name: device.short_name.as_str().into(),
        macaddr: device.mac.to_vec(),
        hw_model: device.hw_model as i32,
        role: device.role as i32,
        ..Default::default()
    }
    .encode_to_vec()
}
