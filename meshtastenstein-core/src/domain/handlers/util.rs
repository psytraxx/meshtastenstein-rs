extern crate alloc;
use crate::{
    domain::{
        context::MeshCtx,
        crypto_psk,
        device::DeviceState,
        node_db::NodeEntry,
        packet::{PacketHeader, PortNumDisplay, RadioFrame},
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

/// Packet-level fields needed to build a `FromRadio` BLE message from a received LoRa frame.
///
/// Passed by reference to [`forward_to_ble`] and [`make_from_radio_packet`] so neither
/// function exceeds the `clippy::too_many_arguments` limit.
pub struct PacketForwardArgs<'a> {
    pub header: &'a PacketHeader,
    pub channel_index: u8,
    pub portnum: i32,
    pub payload: &'a [u8],
    /// Non-zero when the packet is a reply / emoji reaction to another packet's ID.
    pub reply_id: u32,
    /// Non-zero when the payload should be treated as an emoji reaction.
    pub emoji: u32,
    pub meta: RadioMetadata,
}

pub async fn forward_to_ble<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    args: &PacketForwardArgs<'_>,
) {
    let from_radio_id = next_from_radio_id(ctx.from_radio_id);
    let data = make_from_radio_packet(from_radio_id, args);
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

/// Send a Routing error response back to `dest` via LoRa, PSK-encrypted on the
/// primary channel. Used for PKI failures so the sender knows the DM was not
/// received and can show an error or prompt a resend.
pub async fn send_routing_error<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    dest: u32,
    request_id: u32,
    error: routing::Error,
) {
    let payload = Routing {
        variant: Some(routing::Variant::ErrorReason(error as i32)),
    }
    .encode_to_vec();

    let packet_id = ctx.device.next_packet_id();
    if let Some(frame) = (TxBuilder {
        dest,
        portnum: PortNum::RoutingApp.into(),
        inner_payload: payload,
        channel_idx: Some(0), // primary channel
        request_id,
        ..Default::default()
    })
    .build(ctx.device, ctx.router, ctx.node_db, packet_id, None)
    {
        ctx.tx_to_lora.send(frame).await;
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
            ctx.channel_metrics.channel_util,
            ctx.channel_metrics.air_util_tx,
            PortNumDisplay(portnum)
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

/// (Re)issue the admin session passkey if none is set, or the current one has
/// expired.
///
/// Matches upstream `AdminModule` session semantics: 8 bytes drawn from the
/// entropy source (upstream: `session_passkey[i] = random()` per byte), valid
/// for 300 seconds from issue. The previous implementation derived 16
/// deterministic bytes from `my_node_num` — that number is broadcast in every
/// packet header, so anyone overhearing this node could compute a "valid"
/// passkey, and the 16-byte size didn't match what a stock node's
/// `checkPassKey` expects (`size == 8`) in the first place.
pub fn ensure_session_passkey(ctx: &mut MeshCtx<'_, impl MeshStorage>) {
    let needs_new = match ctx.session_passkey {
        Some(existing) => existing.is_expired(),
        None => true,
    };
    if !needs_new {
        return;
    }
    let mut key = [0u8; 8];
    key[0..4].copy_from_slice(&ctx.entropy.random_u32().to_le_bytes());
    key[4..8].copy_from_slice(&ctx.entropy.random_u32().to_le_bytes());
    *ctx.session_passkey = Some(crate::domain::context::SessionPasskey {
        key,
        issued_at: embassy_time::Instant::now(),
    });
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

/// Build the BLE advertised device name: prefix + last 2 MAC bytes as hex,
/// e.g. "Meshtastic_a1b2". Shared by every board's BLE task setup — the only
/// difference between boards was how the resulting `&'static str` got
/// stored (a `StaticCell`), which stays board-side.
pub fn build_ble_device_name(mac: &[u8; 6]) -> heapless::String<24> {
    let mut name: heapless::String<24> = heapless::String::new();
    name.push_str(crate::constants::BLE_DEVICE_NAME_PREFIX).ok();
    for &byte in &mac[4..6] {
        let [hi, lo] = hex_byte(byte);
        name.push(hi).ok();
        name.push(lo).ok();
    }
    name
}

pub fn make_from_radio_packet(
    from_radio_id: u32,
    args: &PacketForwardArgs<'_>,
) -> heapless::Vec<u8, 512> {
    let mesh_pkt = MeshPacket {
        from: args.header.sender,
        to: args.header.destination,
        channel: args.channel_index as u32,
        id: args.header.packet_id,
        rx_snr: args.meta.snr as f32,
        hop_limit: args.header.hop_limit() as u32,
        hop_start: args.header.hop_start() as u32,
        want_ack: args.header.want_ack(),
        rx_rssi: Some(args.meta.rssi as i32),
        payload_variant: Some(mesh_packet::PayloadVariant::Decoded(Data {
            portnum: args.portnum,
            payload: args.payload.to_vec(),
            reply_id: args.reply_id,
            emoji: args.emoji,
            ..Default::default()
        })),
        ..Default::default()
    };
    encode_from_radio(from_radio_id, from_radio::PayloadVariant::Packet(mesh_pkt))
}

pub fn make_node_info_from_radio(from_radio_id: u32, entry: &NodeEntry) -> heapless::Vec<u8, 512> {
    let id = build_node_id_string(entry.node_num);

    // Always include a User — the Meshtastic app ignores NodeInfo with user=None
    // and won't add it to the node list. For stub entries (no real NodeInfo
    // received yet) we synthesise names from the node number, matching the
    // official firmware's behaviour for unknown peers.
    let user = Some(
        entry
            .user
            .as_ref()
            .map(|u| {
                let mut u = u.clone();
                u.id = id.clone();
                u
            })
            .unwrap_or_else(|| {
                // Derive short/long names from the node number: last 4 hex digits.
                let short = alloc::format!("{:04x}", entry.node_num & 0xFFFF);
                let long = alloc::format!("Meshtastic {}", short);
                crate::proto::User {
                    id: id.clone(),
                    long_name: long,
                    short_name: short,
                    ..Default::default()
                }
            }),
    );

    let node_info = crate::proto::NodeInfo {
        num: entry.node_num,
        user,
        position: entry.position,
        // Only report SNR we measured ourselves; an unheard node sends 0.0,
        // which the app would otherwise render as a real 0 dB reading.
        // (rx_snr is proto3 singular, so "unknown" and "0 dB" are genuinely
        // indistinguishable on the wire — upstream notes the same asymmetry.)
        snr: entry.snr.map(f32::from).unwrap_or(0.0),
        last_heard: entry.last_heard,
        // NO device_metrics / position here. Upstream's other-node NodeInfos
        // always go out "thin" (`ConvertToNodeInfoThin` → `ConvertToNodeInfo(
        // lite, nullptr, nullptr)`); the phone learns a peer's battery and
        // position from replayed TELEMETRY_APP / POSITION_APP packets after
        // config-complete instead. See `make_replay_telemetry_packet`.
        hops_away: entry.hops_away.map(u32::from),
        is_favorite: entry.is_favorite,
        is_ignored: entry.is_ignored,
        is_muted: entry.is_muted,
        ..Default::default()
    };
    encode_from_radio(
        from_radio_id,
        from_radio::PayloadVariant::NodeInfo(node_info),
    )
}

/// Queue one `FromRadio` variant into the BLE TX channel, allocating the next
/// monotonic ID. Drops (with a warning) when the channel is full.
///
/// Dropping rather than awaiting a free slot is deliberate. Only the BLE task
/// drains this channel, and only while a phone is connected and subscribed —
/// so a phone that disconnects part-way through a config exchange leaves the
/// rest of that exchange with no reader. Blocking here would stall the mesh
/// orchestrator itself (this runs on its task), taking LoRa receive handling
/// and every periodic broadcast down with it until the watchdog fired. A
/// dropped packet on a connection that no longer exists costs nothing: the
/// phone re-requests the whole config exchange when it reconnects.
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
    if ctx
        .tx_to_ble
        .try_send(FromRadioMessage {
            data: encode_from_radio(id, variant),
            id,
        })
        .is_err()
    {
        warn!("[Mesh] BLE TX queue full, dropped FromRadio id={}", id);
    }
}

/// Decrypt (PSK path only) and decode the `Data` payload from a stored
/// `RadioFrame`. Returns `(portnum, inner_payload, channel_index)` or `None`
/// on decryption/decode failure.
///
/// This is the shared helper used by both `replay_stored_frames` and any future
/// caller that needs to re-decode a previously received frame. It intentionally
/// covers only the PSK path — PKC frames are not stored for replay.
/// Decoded result from a PSK-encrypted RadioFrame.
pub struct DecodedPskFrame {
    pub portnum: i32,
    pub payload: alloc::vec::Vec<u8>,
    pub channel_index: u8,
    pub reply_id: u32,
    pub emoji: u32,
}

pub fn decode_psk_frame(frame: &RadioFrame, device: &DeviceState) -> Option<DecodedPskFrame> {
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
    Some(DecodedPskFrame {
        portnum: data_msg.portnum,
        payload: data_msg.payload,
        channel_index,
        reply_id: data_msg.reply_id,
        emoji: data_msg.emoji,
    })
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

/// Send a local "received"/"queued" confirmation to the phone. This is a
/// receipt of local processing, not a mesh delivery guarantee — used for
/// admin commands (received and about to be acted on) and for LoRa sends
/// that don't request a mesh ACK (there's nothing further to report).
///
/// For a send that *does* request a mesh ACK (`want_ack` and unicast), use
/// [`send_ble_routing_result`] instead once the real outcome is known —
/// sending this unconditionally for those would tell the phone "delivered"
/// before the mesh had even attempted delivery.
pub async fn send_ble_routing_ack<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    dest: u32,
    request_id: u32,
) {
    send_ble_routing_result(ctx, dest, request_id, routing::Error::None).await;
}

/// Send a routing result to the phone for a BLE-originated send that
/// requested a mesh ACK, once the real outcome is known: `routing::Error::None`
/// when a genuine mesh ACK came back (see `from_radio::routing::handle`), or
/// `routing::Error::MaxRetransmit` when retries were exhausted with no ACK
/// (see `MeshRouter::tick_retransmissions`) — matching upstream's own
/// `meshtastic_Routing_Error_MAX_RETRANSMIT` for the same situation.
pub async fn send_ble_routing_result<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    dest: u32,
    request_id: u32,
    error: routing::Error,
) {
    let routing_bytes = Routing {
        variant: Some(routing::Variant::ErrorReason(error as i32)),
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
            "[Admin] BLE TX full, dropped routing result for {:08x}",
            request_id
        );
    }
    debug!(
        "[Admin] BLE routing result ({:?}) sent for request {:08x}",
        error, request_id
    );
}

// =============================================================================
// Satellite-DB replay (upstream `PhoneAPI::makeReplay*Packet`)
// =============================================================================
//
// Other-node NodeInfos go out "thin" — no bundled position or device_metrics
// (`ConvertToNodeInfoThin`). The phone instead learns a peer's battery and
// position from ordinary-looking POSITION_APP / TELEMETRY_APP packets that we
// synthesise from stored state and push *after* ConfigCompleteId, exactly as
// upstream drains its satellite DBs in `STATE_SEND_PACKETS`.
//
// Upstream's phase/prefetch/mutex machinery (REPLAY_PHASE_*, kReplayPrefetchDepth,
// nodeInfoMutex) exists because `getFromRadio` is pull-based — the phone asks for
// one packet at a time, and BLE callbacks run on a separate FreeRTOS task. We push
// into an Embassy channel from the single mesh task, so the ordering those phases
// enforce (positions, then telemetry) is just the order we send in. Environment
// and status phases are omitted: we store neither kind of metric.

/// Stable synthetic packet id, so replaying unchanged history on every reconnect
/// doesn't look like new traffic to the phone's dedup/history. Upstream
/// `makeReplayPacketId`: two rounds of Knuth's multiplicative constant over
/// (node, timestamp, kind), with 0 mapped to 1 since some clients read id 0 as
/// "unset".
fn make_replay_packet_id(num: u32, timestamp: u32, kind: u32) -> u32 {
    let mut h = num;
    h = h.wrapping_mul(2654435761).wrapping_add(timestamp);
    h = h.wrapping_mul(2654435761).wrapping_add(kind);
    if h == 0 { 1 } else { h }
}

/// 2020-01-01. A boot-relative counter needs ~50 years of uptime to reach this,
/// so a `last_heard` at or above it cannot be confused with uptime seconds.
/// Upstream `MIN_PLAUSIBLE_EPOCH` — used to decide whether `rx_time` is a real
/// wall-clock instant worth sending at all.
const MIN_PLAUSIBLE_EPOCH: u32 = 1_577_836_800;

/// Derive `(hop_start, hop_limit)` from a node's last-known hop count.
/// Upstream `setReplayHopFields`: when hops are unknown, send 0/0 rather than
/// fabricating a direct-neighbour reading — `hop_start == 0` means "unknown",
/// not "zero hops away".
fn replay_hop_fields(entry: &NodeEntry, configured_hop_limit: u8) -> (u32, u32) {
    match entry.hops_away {
        None => (0, 0),
        Some(hops) => (
            configured_hop_limit as u32,
            configured_hop_limit.saturating_sub(hops) as u32,
        ),
    }
}

/// Shape a stored `DeviceMetrics` as though the peer had just broadcast it.
/// `to = BROADCAST` is deliberate: addressing it to us would read as a DM from
/// the peer and never reach the node-detail UI.
///
/// `rx_rssi` is deliberately left absent — we store no per-node RSSI, and the
/// field has explicit presence on the wire, so omitting it says "unknown"
/// rather than claiming a real 0 dBm reading.
pub fn make_replay_telemetry_packet(
    from_radio_id: u32,
    entry: &NodeEntry,
    metrics: &crate::proto::DeviceMetrics,
    configured_hop_limit: u8,
) -> heapless::Vec<u8, 512> {
    let rx_time = entry.last_heard;
    let (hop_start, hop_limit) = replay_hop_fields(entry, configured_hop_limit);
    let payload = crate::proto::Telemetry {
        time: rx_time,
        variant: Some(crate::proto::telemetry::Variant::DeviceMetrics(*metrics)),
    }
    .encode_to_vec();

    let pkt = MeshPacket {
        from: entry.node_num,
        to: crate::domain::packet::BROADCAST_ADDR,
        // We track no per-node channel, so replay on the primary channel. Upstream
        // carries the slot each node was heard on (`header->channel`); ours is a
        // single-primary-channel device in practice, and NodeInfo.channel is only
        // populated upstream when it isn't the default channel anyway.
        channel: 0,
        id: make_replay_packet_id(entry.node_num, rx_time, PortNum::TelemetryApp as u32),
        // Only claim a receive time when it's a genuine epoch, not a 0 or an
        // uptime-seconds placeholder. `rx_time` has explicit presence on the
        // wire, so `None` says "unknown" rather than claiming the epoch — this
        // is exactly upstream's `has_rx_time = lastHeardIsWallClock(header)`.
        rx_time: (rx_time >= MIN_PLAUSIBLE_EPOCH).then_some(rx_time),
        rx_snr: entry.snr.map(f32::from).unwrap_or(0.0),
        hop_start,
        hop_limit,
        priority: mesh_packet::Priority::Background as i32,
        payload_variant: Some(mesh_packet::PayloadVariant::Decoded(Data {
            portnum: PortNum::TelemetryApp as i32,
            payload,
            ..Default::default()
        })),
        ..Default::default()
    };
    encode_from_radio(from_radio_id, from_radio::PayloadVariant::Packet(pkt))
}

/// Same idea for a stored position. Shaped like a live broadcast Position so
/// the phone runs it through its normal position-broadcast handling.
pub fn make_replay_position_packet(
    from_radio_id: u32,
    entry: &NodeEntry,
    position: &crate::proto::Position,
    configured_hop_limit: u8,
) -> heapless::Vec<u8, 512> {
    let rx_time = entry.last_heard;
    let (hop_start, hop_limit) = replay_hop_fields(entry, configured_hop_limit);
    let payload = position.encode_to_vec();

    let pkt = MeshPacket {
        from: entry.node_num,
        to: crate::domain::packet::BROADCAST_ADDR,
        channel: 0,
        id: make_replay_packet_id(entry.node_num, rx_time, PortNum::PositionApp as u32),
        rx_time: (rx_time >= MIN_PLAUSIBLE_EPOCH).then_some(rx_time),
        rx_snr: entry.snr.map(f32::from).unwrap_or(0.0),
        hop_start,
        hop_limit,
        priority: mesh_packet::Priority::Background as i32,
        payload_variant: Some(mesh_packet::PayloadVariant::Decoded(Data {
            portnum: PortNum::PositionApp as i32,
            payload,
            ..Default::default()
        })),
        ..Default::default()
    };
    encode_from_radio(from_radio_id, from_radio::PayloadVariant::Packet(pkt))
}

/// Push the stored satellite state (positions, then telemetry) for every known
/// node as synthesised broadcast packets. Called right after ConfigCompleteId,
/// mirroring upstream's post-config_complete_id replay drain.
///
/// Our own node is skipped: the phone already has our position and metrics from
/// the handshake's own-NodeInfo step.
pub async fn replay_satellite_db<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>) {
    let our_num = ctx.device.my_node_num;
    let hop_limit = ctx.device.hop_limit;

    // Collect first: `make_replay_*` borrows the entry immutably while
    // `push_*` needs `&mut ctx`.
    let mut positions: alloc::vec::Vec<(NodeEntry, crate::proto::Position)> =
        alloc::vec::Vec::new();
    let mut telemetry: alloc::vec::Vec<(NodeEntry, crate::proto::DeviceMetrics)> =
        alloc::vec::Vec::new();
    for entry in ctx.node_db.iter() {
        if entry.node_num == our_num {
            continue;
        }
        if let Some(pos) = entry.position {
            positions.push((entry.clone(), pos));
        }
        if let Some(dm) = entry.device_metrics {
            telemetry.push((entry.clone(), dm));
        }
    }

    let (pos_count, tel_count) = (positions.len(), telemetry.len());
    for (entry, pos) in &positions {
        let id = next_from_radio_id(ctx.from_radio_id);
        let data = make_replay_position_packet(id, entry, pos, hop_limit);
        if ctx
            .tx_to_ble
            .try_send(FromRadioMessage { data, id })
            .is_err()
        {
            warn!(
                "[Mesh] BLE TX queue full, dropped replay position id={}",
                id
            );
        }
    }
    for (entry, dm) in &telemetry {
        let id = next_from_radio_id(ctx.from_radio_id);
        let data = make_replay_telemetry_packet(id, entry, dm, hop_limit);
        if ctx
            .tx_to_ble
            .try_send(FromRadioMessage { data, id })
            .is_err()
        {
            warn!(
                "[Mesh] BLE TX queue full, dropped replay telemetry id={}",
                id
            );
        }
    }
    if pos_count > 0 || tel_count > 0 {
        debug!(
            "[Mesh] Satellite replay: {} position(s), {} telemetry",
            pos_count, tel_count
        );
    }
}
