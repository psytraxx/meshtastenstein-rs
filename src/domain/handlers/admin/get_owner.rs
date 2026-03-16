//! Handler for AdminMessage::GetOwnerRequest

use super::{AdminContext, AdminResult};
use crate::proto::{User, admin_message};
use log::info;

#[allow(deprecated)] // User::macaddr is deprecated in proto but still the only API
pub fn handle(ctx: &AdminContext<'_>) -> AdminResult {
    info!(
        "[Admin] GetOwnerRequest → responding with {}",
        ctx.node_id_str
    );
    AdminResult {
        response: Some(admin_message::PayloadVariant::GetOwnerResponse(User {
            id: ctx.node_id_str.into(),
            long_name: ctx.device.long_name.as_str().into(),
            short_name: ctx.device.short_name.as_str().into(),
            macaddr: ctx.device.mac.to_vec(),
            hw_model: ctx.device.hw_model as i32,
            role: ctx.device.role as i32,
            ..Default::default()
        })),
        ..AdminResult::default()
    }
}
