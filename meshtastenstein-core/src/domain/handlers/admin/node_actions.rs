//! Node-list admin actions: favorite/ignore/mute, AddContact, fixed position.
//!
//! These mutate `NodeDB` entries or `MeshCtx::my_position_bytes` directly; none
//! of them send an admin response (matching upstream, which treats these as
//! fire-and-forget setters with no `*_response` payload variant).

use crate::{
    domain::context::MeshCtx,
    ports::MeshStorage,
    proto::{Position, SharedContact},
};
use log::info;
use prost::Message;

pub async fn handle_set_favorite<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, node_num: u32) {
    info!("[Admin] Setting node {:08x} as favorite", node_num);
    ctx.node_db.set_favorite(node_num, true);
}

pub async fn handle_remove_favorite<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, node_num: u32) {
    info!("[Admin] Removing node {:08x} as favorite", node_num);
    ctx.node_db.set_favorite(node_num, false);
}

pub async fn handle_set_ignored<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, node_num: u32) {
    info!("[Admin] Setting node {:08x} as ignored", node_num);
    ctx.node_db.set_ignored(node_num, true);
}

pub async fn handle_remove_ignored<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, node_num: u32) {
    info!("[Admin] Removing node {:08x} as ignored", node_num);
    ctx.node_db.set_ignored(node_num, false);
}

pub async fn handle_toggle_muted<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, node_num: u32) {
    info!("[Admin] Toggling mute for node {:08x}", node_num);
    ctx.node_db.toggle_muted(node_num);
}

pub async fn handle_add_contact<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, contact: SharedContact) {
    let Some(user) = contact.user else {
        return;
    };
    info!("[Admin] Adding contact {:08x}", contact.node_num);
    let Some(entry) = ctx.node_db.get_or_create(contact.node_num) else {
        return;
    };
    entry.user = Some(user);
    // `get_or_create` only marks dirty when it *creates* the entry, but this
    // handler also mutates existing entries (re-sharing a contact, updating
    // its ignore/favorite status) — mark dirty unconditionally so those
    // updates aren't silently dropped from the next flush.
    ctx.node_db.mark_dirty();
    if contact.should_ignore {
        // Matches upstream: should_ignore scrubs cached state and marks ignored,
        // overriding any favorite status.
        ctx.node_db.set_ignored(contact.node_num, true);
        ctx.node_db.set_favorite(contact.node_num, false);
    } else {
        // Matches upstream: mark as favorite so this contact isn't immediately
        // evicted as a "boring old node" (favorite && last_heard==0 survives
        // eviction; a freshly-added contact has last_heard==0).
        ctx.node_db.set_favorite(contact.node_num, true);
    }
}

pub async fn handle_set_fixed_position<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    position: Position,
) {
    info!("[Admin] Setting fixed position");
    ctx.my_position_bytes.clear();
    let bytes = position.encode_to_vec();
    ctx.my_position_bytes.extend_from_slice(&bytes).ok();
}

pub async fn handle_remove_fixed_position<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>) {
    info!("[Admin] Removing fixed position");
    ctx.my_position_bytes.clear();
}
