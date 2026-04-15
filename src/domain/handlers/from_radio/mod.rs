//! Dispatch for packets arriving from the LoRa radio.
//!
//! `dispatch()` is async and performs side effects directly via `MeshCtx`.
//!
//! # How to add a new LoRa portnum handler
//! 1. Create `from_radio/my_portnum.rs` with `pub async fn handle<S: MeshStorage>(ctx, pkt: &super::InboundPacket<'_>)`
//! 2. Add `pub mod my_portnum;` here
//! 3. Add a match arm: `Some(PortNum::MyPortnum) => my_portnum::handle(ctx, &inbound).await`
//! 4. The handler receives all decoded packet fields via `pkt.*`; may call `forward_to_ble`, `send_routing_ack`, or update `ctx` state

extern crate alloc;
pub mod neighbor_info;
pub mod node_info;
pub mod position;
pub mod remote_hardware;
pub mod routing;
pub mod telemetry;
pub mod text_message;
pub mod traceroute;
pub mod waypoint;

use crate::{
    domain::{
        context::MeshCtx,
        crypto_pkc::{PKC_OVERHEAD, decrypt_pkc, derive_shared_key, keypair_from_seed},
        crypto_psk,
        handlers::util::{forward_to_ble, send_routing_ack},
        packet::{BROADCAST_ADDR, RadioFrame},
        router::{FilterResult, PendingRebroadcast},
    },
    inter_task::channels::{LedCommand, LedPattern, RadioMetadata},
    ports::MeshStorage,
    proto::{Data, PortNum},
};
use embassy_time::{Duration, Instant};
use log::{debug, info, warn};
use prost::Message;
use x25519_dalek;

/// Decoded, decrypted fields of a single inbound LoRa packet, ready for portnum handlers.
pub struct InboundPacket<'a> {
    pub sender: u32,
    pub packet_id: u32,
    pub relay_node: u8,
    pub payload: &'a [u8],
    pub addressed_to_us: bool,
    pub want_response: bool,
    pub request_id: u32,
    pub channel_idx: u8,
    pub snr: i8,
}

/// Result of decrypting and proto-decoding a raw LoRa frame payload.
struct DecodedPayload {
    portnum: i32,
    want_response: bool,
    request_id: u32,
    /// Decoded (and decrypted) inner payload bytes.
    payload: alloc::vec::Vec<u8>,
    /// Resolved channel index (0 if unknown).
    channel_index: u8,
}

/// Decrypt and proto-decode the payload of an inbound frame.
///
/// Tries PKC first (when conditions are met), falls back to PSK, then passes
/// unencrypted payloads through unchanged. Returns `None` when decryption fails
/// so the caller can drop the frame.
fn try_decrypt_and_decode(
    frame: &RadioFrame,
    header: &crate::domain::packet::PacketHeader,
    device: &crate::domain::device::DeviceState,
    node_db: &crate::domain::node_db::NodeDB,
    pkc_priv_bytes: &[u8; 32],
) -> Option<DecodedPayload> {
    let preset_name = device.modem_preset.display_name();
    let channel = device
        .channels
        .find_by_hash(header.channel_index, preset_name);
    let channel_index = channel.map(|c| c.index).unwrap_or(0);

    let raw_payload = frame.payload();
    let mut payload = heapless::Vec::<u8, 256>::new();
    payload.extend_from_slice(raw_payload).ok();

    // PKC path: channel_hash == 0, unicast to us, sender has a stored public key,
    // payload is large enough for PKC overhead, and we have a non-zero private key.
    let is_unicast_to_us = header.destination == device.my_node_num;
    let sender_pub_key = node_db.get(header.sender).and_then(|e| e.pub_key);
    let try_pkc = header.channel_index == 0
        && is_unicast_to_us
        && raw_payload.len() > PKC_OVERHEAD
        && sender_pub_key.is_some()
        && pkc_priv_bytes.iter().any(|&b| b != 0);

    if try_pkc {
        let peer_pub_key = sender_pub_key?;
        let (my_secret, _) = keypair_from_seed(*pkc_priv_bytes);
        let peer_pub = x25519_dalek::PublicKey::from(peer_pub_key);
        let shared_key = derive_shared_key(&my_secret, &peer_pub);
        let plaintext_len = raw_payload.len().saturating_sub(PKC_OVERHEAD);
        let mut plain_buf = [0u8; 256];
        if plaintext_len > plain_buf.len() {
            warn!("[Mesh] PKC payload too large from {:08x}", header.sender);
            return None;
        }
        match decrypt_pkc(
            &shared_key,
            header.packet_id,
            header.sender,
            raw_payload,
            &mut plain_buf[..plaintext_len],
        ) {
            Ok(n) => {
                payload.clear();
                payload.extend_from_slice(&plain_buf[..n]).ok();
                info!(
                    "[Mesh] PKC decrypted {} bytes from {:08x}",
                    n, header.sender
                );
            }
            Err(_) => {
                warn!(
                    "[Mesh] PKC decrypt failed from {:08x}, dropping",
                    header.sender
                );
                return None;
            }
        }
    } else if let Some(ch) = channel
        && ch.is_encrypted()
        && !payload.is_empty()
    {
        let (psk_copy, psk_len) = crypto_psk::copy_psk(ch.effective_psk());
        if crypto_psk::crypt_packet(
            &psk_copy[..psk_len],
            header.packet_id,
            header.sender,
            &mut payload,
        )
        .is_err()
        {
            warn!(
                "[Mesh] Decryption failed for channel hash=0x{:02x}",
                header.channel_index
            );
            return None;
        }
        info!(
            "[Mesh] Decrypted {} bytes with ch_hash=0x{:02x}",
            payload.len(),
            header.channel_index
        );
    }

    let data_msg = Data::decode(payload.as_slice())
        .map_err(|e| warn!("[Mesh] Could not decode Data message: {:?}", e))
        .ok()?;

    Some(DecodedPayload {
        portnum: data_msg.portnum,
        want_response: data_msg.want_response,
        request_id: data_msg.request_id,
        payload: data_msg.payload,
        channel_index,
    })
}

pub async fn dispatch<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    frame: RadioFrame,
    metadata: RadioMetadata,
) {
    let header = match frame.header() {
        Some(h) => h,
        None => {
            warn!("[Mesh] Invalid frame header");
            return;
        }
    };

    info!(
        "[Mesh] RX: from={:08x} to={:08x} id={:08x} ch=0x{:02x} hop={}/{} next_hop=0x{:02x} relay=0x{:02x} rssi={} snr={}",
        header.sender,
        header.destination,
        header.packet_id,
        header.channel_index,
        header.hop_limit(),
        header.hop_start(),
        header.next_hop,
        header.relay_node,
        metadata.rssi,
        metadata.snr,
    );

    // =========================================================================
    // Layer 0: Own-packet check → implicit ACK
    // =========================================================================
    if header.sender == ctx.device.my_node_num {
        let idx = ctx
            .pending_packets
            .iter()
            .position(|a| a.packet_id == header.packet_id);
        if let Some(i) = idx {
            info!(
                "[Mesh] Implicit ACK: heard rebroadcast of {:08x}",
                header.packet_id
            );
            ctx.pending_packets.swap_remove(i);
        }
        debug!("[Mesh] Own packet rebroadcast heard, dropping");
        return;
    }

    // =========================================================================
    // Layer 1: FloodingRouter — duplicate detection + upgrade + relay cancel
    // =========================================================================
    let now_ms = Instant::now().as_ticks() * 1_000 / embassy_time::TICK_HZ;

    // Get the hop_limit of our pending rebroadcast for this packet (if any)
    let pending_hop_limit = ctx.pending_rebroadcast.as_ref().and_then(|p| {
        let ph = p.frame.header()?;
        if ph.sender == header.sender && ph.packet_id == header.packet_id {
            Some(ph.hop_limit())
        } else {
            None
        }
    });

    match ctx.router.should_filter_received(
        header.sender,
        header.packet_id,
        header.hop_limit(),
        header.relay_node,
        now_ms,
        pending_hop_limit,
    ) {
        FilterResult::New => {
            // Process normally — fall through
        }
        FilterResult::DuplicateUpgrade(new_hop) => {
            // Upgrade the pending rebroadcast with better hop_limit
            if let Some(pending) = ctx.pending_rebroadcast.as_mut() {
                let relay_node = (ctx.device.my_node_num & 0xFF) as u8;
                pending.frame = frame.with_rewritten_header(new_hop, relay_node);
                info!(
                    "[Mesh] Duplicate upgrade: {:08x} hop_limit -> {}",
                    header.packet_id, new_hop
                );
            }
            return;
        }
        FilterResult::DuplicateCancelRelay => {
            // Another node already relayed this — cancel our pending rebroadcast
            if let Some(p) = ctx.pending_rebroadcast.as_ref()
                && let Some(ph) = p.frame.header()
                && ph.sender == header.sender
                && ph.packet_id == header.packet_id
            {
                debug!(
                    "[Mesh] Cancelling rebroadcast of {:08x} (relayed by 0x{:02x})",
                    header.packet_id, header.relay_node
                );
                *ctx.pending_rebroadcast = None;
            }
            return;
        }
        FilterResult::DuplicateDrop => {
            debug!("[Mesh] Duplicate packet, dropping");
            return;
        }
    }

    let _ = ctx
        .led_commands
        .try_send(LedCommand::Blink(LedPattern::SingleBlink));

    // Update NodeDB (including hops_away from hop_start - hop_limit)
    ctx.node_db.touch(header.sender, 0, metadata.snr, now_ms);
    if let Some(entry) = ctx.node_db.get_mut(header.sender) {
        entry.hops_away = header.hop_start().saturating_sub(header.hop_limit());
    }

    // Decrypt and decode — PKC or PSK, then Data protobuf.
    let decoded = match try_decrypt_and_decode(
        &frame,
        &header,
        ctx.device,
        ctx.node_db,
        ctx.pkc_priv_bytes,
    ) {
        Some(d) => d,
        None => return,
    };
    let portnum = decoded.portnum;
    let want_response = decoded.want_response;
    let request_id = decoded.request_id;
    let inner_payload = decoded.payload;
    let channel_index = decoded.channel_index;
    info!(
        "[Mesh] Decoded portnum={} payload={}B from={:08x}",
        portnum,
        inner_payload.len(),
        header.sender
    );

    let addressed_to_us = header.is_for_us(ctx.device.my_node_num);
    let inbound = InboundPacket {
        sender: header.sender,
        packet_id: header.packet_id,
        relay_node: header.relay_node,
        payload: &inner_payload,
        addressed_to_us,
        want_response,
        request_id,
        channel_idx: channel_index,
        snr: metadata.snr,
    };

    // Store text messages for replay when BLE reconnects
    if (portnum == PortNum::TextMessageApp as i32
        || portnum == PortNum::TextMessageCompressedApp as i32)
        && !*ctx.ble_connected
    {
        let _ = ctx.storage.add(&frame);
        info!("[Mesh] Buffered TEXT_MESSAGE from {:08x}", inbound.sender);
    }

    // =========================================================================
    // Layer 2: Portnum dispatch
    // =========================================================================
    match PortNum::try_from(portnum).ok() {
        Some(PortNum::RemoteHardwareApp) => remote_hardware::handle(ctx, &inbound).await,
        Some(PortNum::TextMessageApp | PortNum::TextMessageCompressedApp) => {
            text_message::handle(ctx, &inbound).await;
        }
        Some(PortNum::PositionApp) => position::handle(ctx, &inbound).await,
        Some(PortNum::NodeinfoApp) => node_info::handle(ctx, &inbound).await,
        Some(PortNum::RoutingApp) => routing::handle(ctx, &inbound).await,
        Some(PortNum::AdminApp) => {
            if inbound.addressed_to_us {
                crate::domain::handlers::admin::dispatch(
                    ctx,
                    inbound.sender,
                    inbound.packet_id,
                    inbound.payload,
                )
                .await;
            } else {
                // Admin packets for others are forwarded to BLE as normal
                forward_to_ble(
                    ctx,
                    &header,
                    channel_index,
                    portnum,
                    inbound.payload,
                    metadata,
                )
                .await;
            }
        }
        Some(PortNum::WaypointApp) => waypoint::handle(ctx, &inbound).await,
        Some(PortNum::TelemetryApp) => telemetry::handle(ctx, &inbound).await,
        Some(PortNum::TracerouteApp) => traceroute::handle(ctx, &inbound).await,
        Some(PortNum::NeighborinfoApp) => neighbor_info::handle(ctx, &inbound).await,
        _ => {
            warn!(
                "[PortHandler] Unknown portnum {} from {:08x}",
                portnum, inbound.sender
            );
        }
    }

    // Default: Forward to BLE if not AdminApp for us
    if (portnum != PortNum::AdminApp as i32 || !inbound.addressed_to_us) && *ctx.ble_connected {
        forward_to_ble(
            ctx,
            &header,
            channel_index,
            portnum,
            inbound.payload,
            metadata,
        )
        .await;
    }

    // Send ACK if addressed to us and want_ack set (on same channel as received)
    if inbound.addressed_to_us && header.want_ack() {
        send_routing_ack(ctx, inbound.sender, inbound.packet_id, channel_index).await;
    }

    // =========================================================================
    // Layer 3: Rebroadcast decision (FloodingRouter + NextHopRouter)
    // =========================================================================
    let is_broadcast = header.destination == BROADCAST_ADDR;

    if should_rebroadcast_for_role(ctx.device.role)
        && let Some(new_hop) = ctx
            .router
            .should_rebroadcast(header.hop_limit(), header.sender)
    {
        // For directed (non-broadcast) packets, only relay if we're the designated next_hop
        if !is_broadcast
            && !ctx
                .router
                .should_relay_directed(header.destination, header.next_hop)
        {
            debug!(
                "[Mesh] Not relaying directed packet {:08x} (not next_hop)",
                header.packet_id
            );
        } else {
            let relay_node = (ctx.device.my_node_num & 0xFF) as u8;
            let rebroadcast_frame = frame.with_rewritten_header(new_hop, relay_node);
            let delay = ctx.router.rebroadcast_delay_ms(metadata.snr);
            *ctx.pending_rebroadcast = Some(PendingRebroadcast {
                frame: rebroadcast_frame,
                deadline: Instant::now() + Duration::from_millis(delay),
            });
            debug!("[Mesh] Scheduling rebroadcast in {}ms", delay);
        }
    }
}

fn should_rebroadcast_for_role(role: crate::domain::device::DeviceRole) -> bool {
    use crate::domain::device::DeviceRole;
    !matches!(role, DeviceRole::ClientMute | DeviceRole::ClientHidden)
}
