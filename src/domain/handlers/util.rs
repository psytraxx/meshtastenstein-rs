use crate::constants::*;
use crate::domain::context::MeshCtx;
use crate::domain::crypto;
use crate::domain::node_db::NodeEntry;
use crate::domain::packet::{PacketHeader, RadioFrame};
use crate::inter_task::channels::{FromRadioMessage, RadioMetadata};
use crate::ports::MeshStorage;
use crate::proto::{
    Data, FromRadio, MeshPacket, PortNum, Routing, from_radio, mesh_packet, routing,
};
use log::{debug, warn};
use prost::Message;

pub async fn forward_to_ble<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    header: &PacketHeader,
    channel_index: u8,
    portnum: i32,
    payload: &[u8],
    meta: RadioMetadata,
) {
    let from_radio_id = next_from_radio_id(ctx.from_radio_id);
    let data = make_from_radio_packet(from_radio_id, header, channel_index, portnum, payload, meta);
    if ctx
        .tx_to_ble
        .try_send(FromRadioMessage {
            data,
            id: from_radio_id,
        })
        .is_err()
    {
        warn!(
            "[Mesh] BLE TX queue full, dropped FromRadio id={}",
            from_radio_id
        );
    }
}

pub async fn notify_ble_node_update<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, sender: u32) {
    let from_radio_id = next_from_radio_id(ctx.from_radio_id);
    if let Some(entry) = ctx.node_db.get(sender) {
        let data = make_node_info_from_radio(from_radio_id, entry);
        if ctx
            .tx_to_ble
            .try_send(FromRadioMessage {
                data,
                id: from_radio_id,
            })
            .is_err()
        {
            warn!(
                "[Mesh] BLE TX queue full, dropped NodeInfo id={}",
                from_radio_id
            );
        }
    }
}

pub async fn send_routing_ack<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    dest: u32,
    request_id: u32,
) {
    debug!(
        "[Mesh] Sending ACK to {:08x} for packet {:08x}",
        dest, request_id
    );

    // Empty Routing payload = ACK success
    let mut enc_buf = Data {
        portnum: PortNum::RoutingApp as i32,
        request_id,
        ..Default::default()
    }
    .encode_to_vec();

    let packet_id = ctx.device.next_packet_id();

    if let Some(ch) = ctx.device.channels.primary()
        && ch.is_encrypted()
    {
        let (psk_copy, psk_len) = crypto::copy_psk(ch.effective_psk());
        let _ = crypto::crypt_packet(&psk_copy[..psk_len], packet_id, ctx.device.my_node_num, &mut enc_buf);
    }

    let channel_hash = ctx
        .device
        .channels
        .primary()
        .map(|c| c.hash(ctx.device.modem_preset.display_name()))
        .unwrap_or(0);

    let header = PacketHeader {
        destination: dest,
        sender: ctx.device.my_node_num,
        packet_id,
        flags: PacketHeader::make_flags(false, false, DEFAULT_HOP_LIMIT, DEFAULT_HOP_LIMIT),
        channel_index: channel_hash,
        next_hop: 0,
        relay_node: 0,
    };

    if let Some(frame) = RadioFrame::from_parts(&header, &enc_buf) {
        ctx.tx_to_lora.send(frame).await;
    }
}

pub async fn send_nodeinfo<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    dest: u32,
    want_response: bool,
) {
    let payload =
        crate::domain::handlers::outgoing::node_info::build_payload(ctx.device, ctx.node_id_str);
    if lora_send(
        ctx,
        PortNum::NodeinfoApp as i32,
        payload,
        dest,
        want_response,
    )
    .await
    {
        debug!("[Mesh] NodeInfo TX: to={:08x}", dest);
    }
}

pub async fn lora_send<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    portnum: i32,
    payload: alloc::vec::Vec<u8>,
    dest: u32,
    want_response: bool,
) -> bool {
    let packet_id = ctx.device.next_packet_id();
    let mut data_bytes = Data {
        portnum,
        payload,
        want_response,
        ..Default::default()
    }
    .encode_to_vec();

    let preset_name = ctx.device.modem_preset.display_name();
    let channel = ctx.device.channels.primary();
    let channel_hash = channel.map(|c| c.hash(preset_name)).unwrap_or(0);

    if let Some(ch) = channel
        && ch.is_encrypted()
    {
        let (psk_copy, psk_len) = crypto::copy_psk(ch.effective_psk());
        let _ = crypto::crypt_packet(&psk_copy[..psk_len], packet_id, ctx.device.my_node_num, &mut data_bytes);
    }

    let header = PacketHeader {
        destination: dest,
        sender: ctx.device.my_node_num,
        packet_id,
        flags: PacketHeader::make_flags(false, false, DEFAULT_HOP_LIMIT, DEFAULT_HOP_LIMIT),
        channel_index: channel_hash,
        next_hop: 0,
        relay_node: 0,
    };

    if let Some(frame) = RadioFrame::from_parts(&header, &data_bytes) {
        ctx.tx_to_lora.send(frame).await;
        true
    } else {
        false
    }
}

pub fn ensure_session_passkey(ctx: &mut MeshCtx<'_, impl MeshStorage>) {
    if ctx.session_passkey.is_some() {
        return;
    }
    let n = ctx.device.my_node_num;
    let mut key = [0u8; 16];
    for (i, &mult) in [0x9E37_79B9u32, 0x6C62_272E, 0xC2B2_AE35, 0x27D4_EB2F]
        .iter()
        .enumerate()
    {
        key[i * 4..(i + 1) * 4].copy_from_slice(&n.wrapping_mul(mult).to_le_bytes());
    }
    *ctx.session_passkey = Some(key);
    debug!("[Admin] Session passkey generated");
}

pub fn next_from_radio_id(from_radio_id: &mut u32) -> u32 {
    let id = *from_radio_id;
    *from_radio_id = from_radio_id.wrapping_add(1).max(1);
    id
}

pub fn build_node_id_string(node_num: u32) -> alloc::string::String {
    let hex = b"0123456789abcdef";
    let mut id = alloc::string::String::with_capacity(9);
    id.push('!');
    for i in (0u32..4).rev() {
        let byte = (node_num >> (i * 8)) as u8;
        id.push(hex[(byte >> 4) as usize] as char);
        id.push(hex[(byte & 0xf) as usize] as char);
    }
    id
}

pub fn make_from_radio_packet(
    from_radio_id: u32,
    header: &PacketHeader,
    channel_index: u8,
    portnum: i32,
    payload: &[u8],
    meta: RadioMetadata,
) -> heapless::Vec<u8, 512> {
    let mesh_pkt = MeshPacket {
        from: header.sender,
        to: header.destination,
        channel: channel_index as u32,
        id: header.packet_id,
        rx_snr: meta.snr as f32,
        hop_limit: header.hop_limit() as u32,
        hop_start: header.hop_start() as u32,
        want_ack: header.want_ack(),
        rx_rssi: meta.rssi as i32,
        payload_variant: Some(mesh_packet::PayloadVariant::Decoded(Data {
            portnum,
            payload: payload.to_vec(),
            ..Default::default()
        })),
        ..Default::default()
    };
    encode_from_radio(from_radio_id, from_radio::PayloadVariant::Packet(mesh_pkt))
}

pub fn make_node_info_from_radio(from_radio_id: u32, entry: &NodeEntry) -> heapless::Vec<u8, 512> {
    let id = build_node_id_string(entry.node_num);

    let user = entry.user.as_ref().map(|u| {
        let mut u = u.clone();
        u.id = id;
        u
    });

    let node_info = crate::proto::NodeInfo {
        num: entry.node_num,
        user,
        position: entry.position,
        snr: entry.snr as f32,
        last_heard: entry.last_heard,
        ..Default::default()
    };
    encode_from_radio(
        from_radio_id,
        from_radio::PayloadVariant::NodeInfo(node_info),
    )
}

pub fn encode_from_radio(id: u32, variant: from_radio::PayloadVariant) -> heapless::Vec<u8, 512> {
    let bytes = FromRadio {
        id,
        payload_variant: Some(variant),
    }
    .encode_to_vec();
    let mut out = heapless::Vec::new();
    out.extend_from_slice(&bytes).ok();
    out
}

pub async fn send_ble_routing_ack<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    dest: u32,
    request_id: u32,
) {
    let routing_bytes = Routing {
        variant: Some(routing::Variant::ErrorReason(0)), // 0 = NONE = success
    }
    .encode_to_vec();
    let packet_id = ctx.device.next_packet_id();
    let from_radio_id = next_from_radio_id(ctx.from_radio_id);
    let msg = FromRadioMessage {
        data: encode_from_radio(
            from_radio_id,
            from_radio::PayloadVariant::Packet(MeshPacket {
                from: ctx.device.my_node_num,
                to: dest,
                id: packet_id,
                payload_variant: Some(mesh_packet::PayloadVariant::Decoded(Data {
                    portnum: PortNum::RoutingApp as i32,
                    payload: routing_bytes,
                    request_id,
                    ..Default::default()
                })),
                ..Default::default()
            }),
        ),
        id: from_radio_id,
    };
    if ctx.tx_to_ble.try_send(msg).is_err() {
        warn!(
            "[Admin] BLE TX full, dropped routing ACK for {:08x}",
            request_id
        );
    }
    debug!(
        "[Admin] BLE routing ACK sent for request {:08x}",
        request_id
    );
}
