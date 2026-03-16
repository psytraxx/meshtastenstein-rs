//! Handler for AdminMessage::SetOwner

use super::{AdminContext, AdminResult};
use crate::proto::User;
use log::info;

pub fn handle(ctx: &mut AdminContext<'_>, user: User) -> AdminResult {
    if !user.long_name.is_empty() {
        ctx.device.long_name = heapless::String::new();
        let src = &user.long_name[..user
            .long_name
            .len()
            .min(ctx.device.long_name.capacity() - 1)];
        let _ = ctx.device.long_name.push_str(src);
    }
    if !user.short_name.is_empty() {
        ctx.device.short_name = heapless::String::new();
        let src = &user.short_name[..user
            .short_name
            .len()
            .min(ctx.device.short_name.capacity() - 1)];
        let _ = ctx.device.short_name.push_str(src);
    }
    info!(
        "[Admin] Owner updated: {} ({})",
        ctx.device.long_name.as_str(),
        ctx.device.short_name.as_str()
    );
    AdminResult {
        needs_persist: true,
        ..AdminResult::default()
    }
}
