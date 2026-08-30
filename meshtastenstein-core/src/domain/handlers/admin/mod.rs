//! Central dispatch for AdminMessage payload variants.
//!
//! Each variant has its own submodule with a `handle` function that performs
//! side effects directly via `MeshCtx`.

pub mod get_channel;
pub mod get_config;
pub mod get_module_config;
pub mod get_owner;
pub mod misc;
pub mod node_actions;
pub mod set_channel;
pub mod set_config;
pub mod set_owner;

use crate::{
    domain::{
        context::MeshCtx,
        handlers::util::{ensure_session_passkey, next_from_radio_id},
        tx::TxBuilder,
    },
    inter_task::channels::FromRadioMessage,
    ports::MeshStorage,
    proto::{
        AdminMessage, Data, FromRadio, MeshPacket, PortNum, admin_message, from_radio, mesh_packet,
    },
};
use log::{debug, warn};
use prost::Message;

// Re-export build_lora_config so periodic/config exchange can use it
pub use get_config::build_lora_config;

/// `via_lora` says which transport the request arrived on, so the response
/// (built by [`send_admin_response`]) goes back the way it came instead of
/// always landing on BLE — a remote admin request from another mesh node must
/// get its reply over LoRa, not handed to whatever phone happens to be
/// connected locally.
pub async fn dispatch<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    requester: u32,
    req_pkt_id: u32,
    admin_bytes: &[u8],
    via_lora: bool,
) {
    let admin_msg = match AdminMessage::decode(admin_bytes) {
        Ok(a) => a,
        Err(e) => {
            warn!("[Admin] Decode failed: {:?}", e);
            return;
        }
    };

    ensure_session_passkey(ctx);

    // Empty passkey is allowed from BLE only (the legacy-app / first-message
    // case, where the phone hasn't been handed a session key yet). Over LoRa
    // there is no equivalent "local, already-trusted" channel, so an empty
    // passkey there is rejected outright rather than silently accepted —
    // otherwise the passkey check does nothing for the transport it matters
    // most on.
    if admin_msg.session_passkey.is_empty() {
        if via_lora {
            warn!("[Admin] Empty session passkey over LoRa, dropping command");
            return;
        }
    } else {
        let expected = ctx
            .session_passkey
            .as_ref()
            .map(|k| k.key.to_vec())
            .unwrap_or_default();
        if admin_msg.session_passkey != expected {
            warn!("[Admin] Session passkey mismatch, dropping command");
            return;
        }
    }

    match admin_msg.payload_variant {
        Some(admin_message::PayloadVariant::GetOwnerRequest(_)) => {
            get_owner::handle(ctx, requester, req_pkt_id, via_lora).await;
        }
        Some(admin_message::PayloadVariant::GetConfigRequest(config_type)) => {
            if let Ok(config_enum) = admin_message::ConfigType::try_from(config_type) {
                get_config::handle(ctx, requester, req_pkt_id, config_enum, via_lora).await;
            } else {
                warn!("[Admin] Invalid config_type: {}", config_type);
            }
        }
        Some(admin_message::PayloadVariant::GetChannelRequest(idx_plus_1)) => {
            get_channel::handle(ctx, requester, req_pkt_id, idx_plus_1, via_lora).await;
        }
        Some(admin_message::PayloadVariant::GetModuleConfigRequest(config_type)) => {
            get_module_config::handle(ctx, requester, req_pkt_id, config_type, via_lora).await;
        }
        Some(admin_message::PayloadVariant::SetOwner(user)) => {
            set_owner::handle(ctx, user).await;
        }
        Some(admin_message::PayloadVariant::SetConfig(cfg)) => {
            set_config::handle(ctx, cfg).await;
        }
        Some(admin_message::PayloadVariant::SetModuleConfig(cfg)) => {
            misc::handle_set_module_config(ctx, cfg).await;
        }
        Some(admin_message::PayloadVariant::SetChannel(ch)) => {
            set_channel::handle(ctx, ch).await;
        }
        Some(admin_message::PayloadVariant::SetFavoriteNode(node_num)) => {
            node_actions::handle_set_favorite(ctx, node_num).await;
        }
        Some(admin_message::PayloadVariant::RemoveFavoriteNode(node_num)) => {
            node_actions::handle_remove_favorite(ctx, node_num).await;
        }
        Some(admin_message::PayloadVariant::SetIgnoredNode(node_num)) => {
            node_actions::handle_set_ignored(ctx, node_num).await;
        }
        Some(admin_message::PayloadVariant::RemoveIgnoredNode(node_num)) => {
            node_actions::handle_remove_ignored(ctx, node_num).await;
        }
        Some(admin_message::PayloadVariant::ToggleMutedNode(node_num)) => {
            node_actions::handle_toggle_muted(ctx, node_num).await;
        }
        Some(admin_message::PayloadVariant::AddContact(contact)) => {
            node_actions::handle_add_contact(ctx, contact).await;
        }
        Some(admin_message::PayloadVariant::SetFixedPosition(position)) => {
            node_actions::handle_set_fixed_position(ctx, position).await;
        }
        Some(admin_message::PayloadVariant::RemoveFixedPosition(_)) => {
            node_actions::handle_remove_fixed_position(ctx).await;
        }
        Some(admin_message::PayloadVariant::BeginEditSettings(_)) => {
            misc::handle_begin_edit(ctx, requester, req_pkt_id, via_lora).await;
        }
        Some(admin_message::PayloadVariant::CommitEditSettings(_)) => {
            misc::handle_commit_edit(ctx, requester, req_pkt_id, via_lora).await;
        }
        Some(admin_message::PayloadVariant::RebootSeconds(secs)) => {
            misc::handle_reboot(ctx, secs as u32).await;
        }
        Some(admin_message::PayloadVariant::FactoryResetConfig(_)) => {
            misc::handle_factory_reset(ctx).await;
        }
        Some(admin_message::PayloadVariant::NodedbReset(_)) => {
            misc::handle_nodedb_reset(ctx).await;
        }
        Some(admin_message::PayloadVariant::ShutdownSeconds(secs)) => {
            misc::handle_shutdown(ctx, secs as u32).await;
        }
        Some(admin_message::PayloadVariant::RemoveByNodenum(node_num)) => {
            misc::handle_remove_node(ctx, node_num).await;
        }
        _ => {
            debug!("[Admin] Unhandled admin variant");
        }
    }
}

/// Send an admin response back to `requester`.
///
/// `via_lora` selects the transport: `false` (the BLE-originated case) sends a
/// `FromRadio` to the locally connected phone, matching the original
/// behavior. `true` (the request arrived over LoRa) instead builds and
/// transmits a LoRa frame addressed to `requester`, so a remote admin
/// request's reply reaches the node that asked — previously every response
/// was sent to BLE regardless, so a LoRa-originated request would apply its
/// state change (e.g. `SetConfig`, `RebootSeconds`) and then appear to the
/// requester as a timeout, since no reply ever went back over the mesh.
pub async fn send_admin_response<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    requester: u32,
    req_pkt_id: u32,
    variant: admin_message::PayloadVariant,
    via_lora: bool,
) {
    let response_bytes = AdminMessage {
        session_passkey: ctx
            .session_passkey
            .as_ref()
            .map(|k| k.key.to_vec())
            .unwrap_or_default(),
        payload_variant: Some(variant),
    }
    .encode_to_vec();

    if via_lora {
        let packet_id = ctx.device.next_packet_id();
        let frame = (TxBuilder {
            dest: requester,
            portnum: PortNum::AdminApp.into(),
            inner_payload: response_bytes,
            request_id: req_pkt_id,
            ..Default::default()
        })
        .build(ctx.device, ctx.router, ctx.node_db, packet_id, None);

        match frame {
            Some(frame) => {
                ctx.tx_to_lora.send(frame).await;
                debug!("[Admin] Response sent to {:08x} over LoRa", requester);
            }
            None => {
                warn!(
                    "[Admin] Failed to build LoRa response frame for {:08x}",
                    requester
                );
            }
        }
        return;
    }

    let packet_id = ctx.device.next_packet_id();
    let from_radio_id = next_from_radio_id(ctx.from_radio_id);

    let from_radio_bytes = FromRadio {
        id: from_radio_id,
        payload_variant: Some(from_radio::PayloadVariant::Packet(MeshPacket {
            from: ctx.device.my_node_num,
            to: requester,
            id: packet_id,
            payload_variant: Some(mesh_packet::PayloadVariant::Decoded(Data {
                portnum: PortNum::AdminApp.into(),
                payload: response_bytes,
                request_id: req_pkt_id,
                ..Default::default()
            })),
            ..Default::default()
        })),
    }
    .encode_to_vec();

    let mut data = heapless::Vec::new();
    data.extend_from_slice(&from_radio_bytes).ok();

    ctx.tx_to_ble
        .send(FromRadioMessage {
            data,
            id: from_radio_id,
        })
        .await;
    debug!("[Admin] Response sent to {:08x} over BLE", requester);
}
