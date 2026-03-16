//! Handler for PortNum::PositionApp

use super::RadioResult;
use crate::mesh::node_db::NodeDB;
use crate::proto::Position as ProtoPosition;
use log::{debug, info, warn};
use prost::Message;

pub fn handle(sender: u32, payload: &[u8], node_db: &mut NodeDB) -> RadioResult {
    debug!(
        "[PortHandler] POSITION from {:08x}: {} bytes",
        sender,
        payload.len()
    );
    match ProtoPosition::decode(payload) {
        Ok(pos) => {
            if let Some(node) = node_db.get_or_create(sender) {
                info!(
                    "[PortHandler] Updated position for {:08x}: lat={} lon={} alt={}",
                    sender,
                    pos.latitude_i.unwrap_or(0) as f64 / 1e7,
                    pos.longitude_i.unwrap_or(0) as f64 / 1e7,
                    pos.altitude.unwrap_or(0)
                );
                node.position = Some(pos);
            }
        }
        Err(e) => warn!(
            "[PortHandler] POSITION decode failed from {:08x}: {:?}",
            sender, e
        ),
    }
    RadioResult {
        notify_ble_of_node_update: true,
        ..RadioResult::default()
    }
}
