//! Central dispatch for AdminMessage payload variants.
//!
//! Each variant has its own submodule with a `handle` function that is pure
//! (no async, no Embassy). `dispatch()` calls the right handler and returns
//! `AdminResult`; `mesh_task` performs the async side-effects (persist, reboot,
//! send BLE response).

pub mod get_channel;
pub mod get_config;
pub mod get_owner;
pub mod misc;
pub mod set_channel;
pub mod set_config;
pub mod set_owner;

use crate::mesh::device::DeviceState;
use crate::proto::admin_message;
use log::debug;

// Re-export build_lora_config so send_config_exchange can use it too
pub use get_config::build_lora_config;

// ── Context ───────────────────────────────────────────────────────────────────

/// State passed to every admin handler.
pub struct AdminContext<'a> {
    pub device: &'a mut DeviceState,
    /// Pre-computed node ID string (e.g. "!deadbeef") for GetOwner responses
    pub node_id_str: &'a str,
}

// ── Result ────────────────────────────────────────────────────────────────────

/// What mesh_task must do after an admin handler returns.
#[derive(Default)]
pub struct AdminResult {
    /// Admin response to send back to the phone. `None` for SET commands
    /// (those get a mesh-level routing ACK instead).
    pub response: Option<admin_message::PayloadVariant>,
    /// Call `persist_config()` before returning.
    pub needs_persist: bool,
    /// Delay this many seconds then call `software_reset()`. `None` = no reboot.
    pub reboot_secs: Option<u64>,
}

// ── Central dispatch ──────────────────────────────────────────────────────────

/// Dispatch an AdminMessage payload variant to the appropriate handler.
///
/// Pure: no async, no Embassy types, no hardware access.
pub fn dispatch(
    ctx: &mut AdminContext<'_>,
    variant: Option<admin_message::PayloadVariant>,
) -> AdminResult {
    match variant {
        Some(admin_message::PayloadVariant::GetOwnerRequest(_)) => get_owner::handle(ctx),

        Some(admin_message::PayloadVariant::GetConfigRequest(config_type)) => {
            get_config::handle(ctx, config_type)
        }

        Some(admin_message::PayloadVariant::GetChannelRequest(idx_plus_1)) => {
            get_channel::handle(ctx, idx_plus_1)
        }

        Some(admin_message::PayloadVariant::SetOwner(user)) => set_owner::handle(ctx, user),

        Some(admin_message::PayloadVariant::SetConfig(cfg)) => set_config::handle(ctx, cfg),

        Some(admin_message::PayloadVariant::SetChannel(ch)) => set_channel::handle(ctx, ch),

        Some(admin_message::PayloadVariant::BeginEditSettings(_)) => misc::handle_begin_edit(),

        Some(admin_message::PayloadVariant::CommitEditSettings(_)) => misc::handle_commit_edit(),

        Some(admin_message::PayloadVariant::RebootSeconds(secs)) => misc::handle_reboot(secs),

        Some(admin_message::PayloadVariant::FactoryResetConfig(_)) => misc::handle_factory_reset(),

        _ => {
            debug!("[Admin] Unhandled admin variant");
            AdminResult::default()
        }
    }
}
