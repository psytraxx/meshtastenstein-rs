//! Handler for PortNum::NodeinfoApp

use super::RadioResult;
use crate::domain::node_db::NodeDB;
use crate::proto::User as ProtoUser;
use log::{debug, info, warn};
use prost::Message;

pub fn handle(
    sender: u32,
    payload: &[u8],
    want_response: bool,
    node_db: &mut NodeDB,
) -> RadioResult {
    debug!(
        "[PortHandler] NODEINFO from {:08x}: {} bytes",
        sender,
        payload.len()
    );
    match ProtoUser::decode(payload) {
        Ok(user) => {
            if let Some(node) = node_db.get_or_create(sender) {
                info!(
                    "[PortHandler] Updated user for {:08x}: {} ({})",
                    sender, &user.long_name, &user.short_name
                );
                node.user = Some(user);
            }
        }
        Err(e) => warn!(
            "[PortHandler] NODEINFO decode failed from {:08x}: {:?}",
            sender, e
        ),
    }
    RadioResult {
        reply_with_nodeinfo: want_response,
        notify_ble_of_node_update: true,
        ..RadioResult::default()
    }
}
