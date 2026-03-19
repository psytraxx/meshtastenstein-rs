use crate::domain::context::MeshCtx;
use crate::ports::MeshStorage;
use crate::proto::{HardwareModel, User};
use log::{info, warn};

pub async fn handle<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, user: User) {
    info!(
        "[Admin] Setting owner: {} ({})",
        user.long_name, user.short_name
    );

    if !user.long_name.is_empty() {
        ctx.device.long_name.clear();
        ctx.device.long_name.push_str(&user.long_name).ok();
    }
    if !user.short_name.is_empty() {
        ctx.device.short_name.clear();
        ctx.device.short_name.push_str(&user.short_name).ok();
    }
    if user.hw_model != 0 {
        if HardwareModel::try_from(user.hw_model).is_ok() {
            ctx.device.hw_model = user.hw_model as u32;
        } else {
            warn!("[Admin] Invalid hw_model: {}", user.hw_model);
        }
    }

    ctx.storage.save_state(ctx.device);
}
