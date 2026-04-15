extern crate alloc;
use crate::{
    domain::{
        context::MeshCtx,
        crypto_psk,
        device::DeviceState,
        node_db::NodeEntry,
        packet::{PacketHeader, RadioFrame},
        radio_config::Region,
        tx::TxBuilder,
    },
    inter_task::channels::{FromRadioMessage, RadioMetadata},
    ports::MeshStorage,
    proto::{Data, FromRadio, MeshPacket, PortNum, Routing, from_radio, mesh_packet, routing},
};
use heapless::Vec;
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
    channel_idx: u8,
) {
    debug!(
        "[Mesh] Sending ACK to {:08x} for packet {:08x} on ch={}",
        dest, request_id, channel_idx
    );
    let packet_id = ctx.device.next_packet_id();
    // Empty inner_payload: the Data.request_id field is the ACK signal
    if let Some(frame) = (TxBuilder {
        dest,
        portnum: PortNum::RoutingApp.into(),
        inner_payload: alloc::vec![],
        channel_idx: Some(channel_idx),
        request_id,
        ..Default::default()
    })
    .build(ctx.device, ctx.router, ctx.node_db, packet_id, None)
    {
        ctx.tx_to_lora.send(frame).await;
    }
}

pub async fn send_nodeinfo<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    dest: u32,
    want_response: bool,
) {
    let payload = crate::domain::handlers::outgoing::node_info::build_payload(
        ctx.device,
        ctx.node_id_str,
        ctx.pkc_pub_bytes,
    );
    if lora_send(
        ctx,
        PortNum::NodeinfoApp.into(),
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
    // Hard regulatory + congestion ceiling. When above the impolite ceiling,
    // drop the frame to stay within the regional duty-cycle budget.
    let region = Region::from_proto(ctx.device.region);
    if !ctx.channel_metrics.tx_allowed_impolite(region) {
        warn!(
            "[Mesh] LoRa TX dropped: above impolite ceiling (ch_util={:.1}% air_tx={:.1}% portnum={})",
            ctx.channel_metrics.channel_util, ctx.channel_metrics.air_util_tx, portnum
        );
        return false;
    }

    let packet_id = ctx.device.next_packet_id();
    let frame = TxBuilder {
        dest,
        portnum,
        inner_payload: payload,
        channel_idx: None, // primary channel
        want_response,
        ..Default::default()
    }
    .build(ctx.device, ctx.router, ctx.node_db, packet_id, None);

    if let Some(frame) = frame {
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

/// Encode a byte as two lowercase hex characters.
pub const fn hex_byte(b: u8) -> [char; 2] {
    const H: &[u8; 16] = b"0123456789abcdef";
    [H[(b >> 4) as usize] as char, H[(b & 0xf) as usize] as char]
}

pub fn build_node_id_string(node_num: u32) -> alloc::string::String {
    let mut id = alloc::string::String::with_capacity(9);
    id.push('!');
    for i in (0u32..4).rev() {
        let [hi, lo] = hex_byte((node_num >> (i * 8)) as u8);
        id.push(hi);
        id.push(lo);
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

/// Queue one `FromRadio` variant into the BLE TX channel, allocating the next
/// monotonic ID. Drops silently (with a warning) when the channel is full.
///
/// This is the canonical one-liner that replaces the repetitive:
/// ```ignore
/// let id = next_from_radio_id(ctx.from_radio_id);
/// ctx.tx_to_ble.send(FromRadioMessage { data: encode_from_radio(id, variant), id }).await;
/// ```
pub async fn push_from_radio<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    variant: from_radio::PayloadVariant,
) {
    let id = next_from_radio_id(ctx.from_radio_id);
    ctx.tx_to_ble
        .send(FromRadioMessage {
            data: encode_from_radio(id, variant),
            id,
        })
        .await;
}

/// Decrypt (PSK path only) and decode the `Data` payload from a stored
/// `RadioFrame`. Returns `(portnum, inner_payload, channel_index)` or `None`
/// on decryption/decode failure.
///
/// This is the shared helper used by both `replay_stored_frames` and any future
/// caller that needs to re-decode a previously received frame. It intentionally
/// covers only the PSK path — PKC frames are not stored for replay.
pub fn decode_psk_frame(
    frame: &RadioFrame,
    device: &DeviceState,
) -> Option<(i32, alloc::vec::Vec<u8>, u8)> {
    let header = frame.header()?;

    let preset_name = device.modem_preset.display_name();
    let channel = device
        .channels
        .find_by_hash(header.channel_index, preset_name);
    let channel_index = channel.map(|c| c.index).unwrap_or(0);

    let mut payload: Vec<u8, 256> = Vec::new();
    payload.extend_from_slice(frame.payload()).ok();

    if let Some(ch) = channel
        && ch.is_encrypted()
        && !payload.is_empty()
    {
        let (psk_copy, psk_len) = crypto_psk::copy_psk(ch.effective_psk());
        crypto_psk::crypt_packet(
            &psk_copy[..psk_len],
            header.packet_id,
            header.sender,
            &mut payload,
        )
        .ok()?;
    }

    let data_msg = Data::decode(payload.as_slice()).ok()?;
    Some((data_msg.portnum, data_msg.payload, channel_index))
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
                    portnum: PortNum::RoutingApp.into(),
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
