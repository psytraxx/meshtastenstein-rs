//! Central dispatch for AdminMessage payload variants.
//!
//! Each variant has its own submodule with a `handle` function that performs
//! side effects directly via `MeshCtx`.

pub mod get_channel;
pub mod get_config;
pub mod get_owner;
pub mod misc;
pub mod set_channel;
pub mod set_config;
pub mod set_owner;

use crate::domain::context::MeshCtx;
use crate::domain::handlers::util::{ensure_session_passkey, next_from_radio_id};
use crate::inter_task::channels::FromRadioMessage;
use crate::ports::MeshStorage;
use crate::proto::{
    AdminMessage, Data, FromRadio, MeshPacket, PortNum, admin_message, from_radio, mesh_packet,
};
use log::{debug, warn};
use prost::Message;

// Re-export build_lora_config so periodic/config exchange can use it
pub use get_config::build_lora_config;

pub async fn dispatch<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    requester: u32,
    req_pkt_id: u32,
    admin_bytes: &[u8],
) {
    let admin_msg = match AdminMessage::decode(admin_bytes) {
        Ok(a) => a,
        Err(e) => {
            warn!("[Admin] Decode failed: {:?}", e);
            return;
        }
    };

    ensure_session_passkey(ctx);

    match admin_msg.payload_variant {
        Some(admin_message::PayloadVariant::GetOwnerRequest(_)) => {
            get_owner::handle(ctx, requester, req_pkt_id).await;
        }
        Some(admin_message::PayloadVariant::GetConfigRequest(config_type)) => {
            if let Ok(config_enum) = admin_message::ConfigType::try_from(config_type) {
                get_config::handle(ctx, requester, req_pkt_id, config_enum).await;
            } else {
                warn!("[Admin] Invalid config_type: {}", config_type);
            }
        }
        Some(admin_message::PayloadVariant::GetChannelRequest(idx_plus_1)) => {
            get_channel::handle(ctx, requester, req_pkt_id, idx_plus_1).await;
        }
        Some(admin_message::PayloadVariant::SetOwner(user)) => {
            set_owner::handle(ctx, user).await;
        }
        Some(admin_message::PayloadVariant::SetConfig(cfg)) => {
            set_config::handle(ctx, cfg).await;
        }
        Some(admin_message::PayloadVariant::SetChannel(ch)) => {
            set_channel::handle(ctx, ch).await;
        }
        Some(admin_message::PayloadVariant::BeginEditSettings(_)) => {
            misc::handle_begin_edit(ctx, requester, req_pkt_id).await;
        }
        Some(admin_message::PayloadVariant::CommitEditSettings(_)) => {
            misc::handle_commit_edit(ctx, requester, req_pkt_id).await;
        }
        Some(admin_message::PayloadVariant::RebootSeconds(secs)) => {
            misc::handle_reboot(secs as u32).await;
        }
        Some(admin_message::PayloadVariant::FactoryResetConfig(_)) => {
            misc::handle_factory_reset(ctx).await;
        }
        Some(admin_message::PayloadVariant::NodedbReset(_)) => {
            misc::handle_nodedb_reset(ctx).await;
        }
        Some(admin_message::PayloadVariant::ShutdownSeconds(secs)) => {
            misc::handle_shutdown(secs as u32).await;
        }
        Some(admin_message::PayloadVariant::RemoveByNodenum(node_num)) => {
            misc::handle_remove_node(ctx, node_num).await;
        }
        _ => {
            debug!("[Admin] Unhandled admin variant");
        }
    }
}

pub async fn send_admin_response<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    requester: u32,
    req_pkt_id: u32,
    variant: admin_message::PayloadVariant,
) {
    let response_bytes = AdminMessage {
        session_passkey: (*ctx.session_passkey)
            .map(|k| k.to_vec())
            .unwrap_or_default(),
        payload_variant: Some(variant),
    }
    .encode_to_vec();

    let packet_id = ctx.device.next_packet_id();
    let from_radio_id = next_from_radio_id(ctx.from_radio_id);

    let from_radio_bytes = FromRadio {
        id: from_radio_id,
        payload_variant: Some(from_radio::PayloadVariant::Packet(MeshPacket {
            from: ctx.device.my_node_num,
            to: requester,
            id: packet_id,
            payload_variant: Some(mesh_packet::PayloadVariant::Decoded(Data {
                portnum: PortNum::AdminApp as i32,
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
    debug!("[Admin] Response sent to {:08x}", requester);
}
