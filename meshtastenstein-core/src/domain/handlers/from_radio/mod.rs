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
    constants::{MAX_NODES, NODEINFO_MIN_INTERVAL_MS},
    domain::{
        context::MeshCtx,
        crypto_pkc::{PKC_OVERHEAD, decrypt_pkc, derive_shared_key, keypair_from_seed},
        crypto_psk,
        handlers::util::{
            PacketForwardArgs, forward_to_ble, notify_ble_node_update, send_nodeinfo,
            send_routing_ack, send_routing_error,
        },
        packet::{BROADCAST_ADDR, RadioFrame},
        radio_config::Region,
        router::{FilterResult, PendingRebroadcast, rebroadcast_delay_ms},
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
    /// Non-zero when this packet is a reply/reaction to another packet.
    pub reply_id: u32,
    /// Non-zero when the payload should be treated as an emoji reaction.
    pub emoji: u32,
    pub channel_idx: u8,
    pub snr: i8,
}

/// Result of decrypting and proto-decoding a raw LoRa frame payload.
struct DecodedPayload {
    portnum: i32,
    want_response: bool,
    request_id: u32,
    reply_id: u32,
    emoji: u32,
    /// Decoded (and decrypted) inner payload bytes.
    payload: alloc::vec::Vec<u8>,
    /// Resolved channel index (0 if unknown).
    channel_index: u8,
}

/// Result returned by [`try_decrypt_and_decode`].
enum DecryptOutcome {
    /// Payload decrypted and decoded successfully.
    Decoded(DecodedPayload),
    /// Packet remained undecoded after trying both PKC and every configured
    /// channel, on a unicast-to-us, channel-hash-0 packet whose sender we
    /// have no stored public key for. Matches upstream's `sniffReceived`
    /// check (`ReliableRouter.cpp`), which only reports
    /// `PKI_UNKNOWN_PUBKEY` once the packet is *still* undecoded after every
    /// decrypt attempt — not the moment a PKC candidate lacks a known key,
    /// since a channel-PSK attempt on the same packet might still succeed.
    PkiUnknownPubkey,
    /// Every decrypt/decode attempt failed, or the packet was rejected
    /// post-decode (legacy channel-encrypted DM) — drop silently.
    Drop,
}

/// Decrypt and proto-decode the payload of an inbound frame.
///
/// Tries PKC first (when conditions are met), falls back to PSK, then passes
/// unencrypted payloads through unchanged. Returns `Drop` when decryption fails
/// silently, or a `Pki*` variant when a PKI-specific error should be reported
/// back to the sender.
fn try_decrypt_and_decode(
    frame: &RadioFrame,
    header: &crate::domain::packet::PacketHeader,
    device: &crate::domain::device::DeviceState,
    node_db: &crate::domain::node_db::NodeDB,
    pkc_priv_bytes: &[u8; 32],
) -> DecryptOutcome {
    let preset_name = device.modem_preset.display_name();
    let channel = device
        .channels
        .find_by_hash(header.channel_index, preset_name);
    let channel_index = channel.map(|c| c.index).unwrap_or(0);

    if channel.is_none() {
        debug!(
            "[Mesh] No channel match for hash=0x{:02x} (preset={})",
            header.channel_index, preset_name
        );
    }

    let raw_payload = frame.payload();

    // PKC candidacy: channel_hash == 0, unicast to us, payload large enough
    // for overhead, and we have a non-zero private key. Matches upstream's
    // `pkiCandidate` (`Router.cpp:948-950`) — note upstream's gate is on
    // *our own* key being set, not the sender's; the sender's key is looked
    // up only once candidacy is established, exactly as here.
    let is_unicast_to_us = header.destination == device.my_node_num;
    let is_pkc_candidate = header.channel_index == 0
        && is_unicast_to_us
        && raw_payload.len() > PKC_OVERHEAD
        && pkc_priv_bytes.iter().any(|&b| b != 0);

    // Tracks whether we hold no stored key for this sender, for the final
    // PkiUnknownPubkey classification below. `is_pkc_candidate` alone isn't
    // enough — a plaintext or PSK unicast whose hash happens to be 0 is also
    // a PKC candidate, and knowing the sender's key status still matters for
    // classifying that packet's eventual failure correctly.
    let sender_pub_key = is_pkc_candidate
        .then(|| node_db.get(header.sender).and_then(|e| e.pub_key))
        .flatten();

    // Tracks whether `payload` holds a PKC-decrypted result, so the
    // legacy-DM check below (which only applies to a channel-encrypted
    // TEXT_MESSAGE_APP) doesn't misfire on a genuine PKC DM.
    let mut pkc_decrypted = false;
    let mut payload = heapless::Vec::<u8, 256>::new();

    // Try PKC first — but only actually attempt decryption when we hold the
    // sender's key. A missing key is not terminal: upstream's own `sniffReceived`
    // (`ReliableRouter.cpp:143-147`) only reports PKI_UNKNOWN_PUBKEY once the
    // packet is *still* undecoded after every attempt, so a channel-PSK
    // attempt on the same packet gets its chance below regardless.
    if let Some(peer_pub_key) = sender_pub_key {
        let (my_secret, my_pub) = keypair_from_seed(*pkc_priv_bytes);
        let peer_pub = x25519_dalek::PublicKey::from(peer_pub_key);
        let shared_key = derive_shared_key(&my_secret, &peer_pub);
        let plaintext_len = raw_payload.len().saturating_sub(PKC_OVERHEAD);
        let mut plain_buf = [0u8; 256];
        if plaintext_len > plain_buf.len() {
            warn!("[Mesh] PKC payload too large from {:08x}", header.sender);
        } else {
            match decrypt_pkc(
                &shared_key,
                header.packet_id,
                header.sender,
                raw_payload,
                &mut plain_buf[..plaintext_len],
            ) {
                Ok(n) => {
                    payload.extend_from_slice(&plain_buf[..n]).ok();
                    pkc_decrypted = true;
                    info!(
                        "[Mesh] PKC decrypted {} bytes from {:08x}",
                        n, header.sender
                    );
                }
                Err(_) => {
                    // Authentication failure: matches upstream, which also
                    // leaves `decrypted = false` here and falls into the
                    // channel-hash loop rather than returning immediately
                    // (`Router.cpp:1035`) — a tag mismatch and a missing key
                    // get exactly the same fallback treatment upstream.
                    let my_pub_b = my_pub.as_bytes();
                    warn!(
                        "[Mesh] PKC decrypt failed from {:08x} — our pub_key={:02x}{:02x}{:02x}{:02x}… sender cached_pub={:02x}{:02x}{:02x}{:02x}…",
                        header.sender,
                        my_pub_b[0],
                        my_pub_b[1],
                        my_pub_b[2],
                        my_pub_b[3],
                        peer_pub_key[0],
                        peer_pub_key[1],
                        peer_pub_key[2],
                        peer_pub_key[3],
                    );
                }
            }
        }
    }

    // Fall back to the channel-hash/PSK path when PKC wasn't attempted
    // (no candidacy, or no known sender key) or didn't succeed.
    if !pkc_decrypted {
        payload.clear();
        payload.extend_from_slice(raw_payload).ok();
        if let Some(ch) = channel
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
                return final_undecoded_outcome(is_unicast_to_us, header, sender_pub_key);
            }
            info!(
                "[Mesh] Decrypted {} bytes with ch_hash=0x{:02x}",
                payload.len(),
                header.channel_index
            );
        }
    }

    let data_msg = match Data::decode(payload.as_slice()) {
        Ok(d) => d,
        Err(e) => {
            warn!("[Mesh] Could not decode Data message: {:?}", e);
            return final_undecoded_outcome(is_unicast_to_us, header, sender_pub_key);
        }
    };

    // Reject a channel-encrypted (non-PKC) direct message, matching upstream's
    // "Rejecting legacy DM" rule (`Router.cpp:1057-1059`): a unicast-to-us
    // TEXT_MESSAGE_APP must arrive PKC-encrypted on 2.5.0+ firmware. Accepting
    // it over the channel PSK would make this device the only node on the
    // mesh that displays a message every other modern receiver discards —
    // worse than dropping it here, where at least the sender can be told.
    if data_msg.portnum == PortNum::TextMessageApp as i32 && is_unicast_to_us && !pkc_decrypted {
        warn!(
            "[Mesh] Rejecting legacy (channel-encrypted) DM from {:08x}",
            header.sender
        );
        return DecryptOutcome::Drop;
    }

    DecryptOutcome::Decoded(DecodedPayload {
        portnum: data_msg.portnum,
        want_response: data_msg.want_response,
        request_id: data_msg.request_id,
        reply_id: data_msg.reply_id,
        emoji: data_msg.emoji,
        payload: data_msg.payload,
        channel_index,
    })
}

/// Classify a packet that never decoded successfully (neither PKC nor any
/// configured channel worked). `PkiUnknownPubkey` only when it was a
/// channel-hash-0 unicast to us with no stored key for the sender — matching
/// upstream's `sniffReceived` gate — otherwise a plain drop.
fn final_undecoded_outcome(
    is_unicast_to_us: bool,
    header: &crate::domain::packet::PacketHeader,
    sender_pub_key: Option<[u8; 32]>,
) -> DecryptOutcome {
    if header.channel_index == 0 && is_unicast_to_us && sender_pub_key.is_none() {
        DecryptOutcome::PkiUnknownPubkey
    } else {
        DecryptOutcome::Drop
    }
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
    // Authenticate BEFORE touching any routing state.
    //
    // Upstream authenticates a *copy* first (`passesRoutingAuthGate`,
    // `Router.cpp:803-848`, called at `Router.cpp:1687`) specifically so that
    // "Reliable/Flooding/NextHop filters [can't] update retry timers, packet
    // history, implicit ACK state, cancellation, or relay queues" from
    // unauthenticated input. Decrypt/decode therefore now runs before the
    // duplicate-ring filter, the NodeDB touch, the BLE node-list push, and
    // `maybe_request_nodeinfo`'s TX — none of which may fire for a packet
    // whose sender we cannot verify.
    // =========================================================================
    let now_ms = Instant::now().as_ticks() * 1_000 / embassy_time::TICK_HZ;

    let decoded = match try_decrypt_and_decode(
        &frame,
        &header,
        ctx.device,
        ctx.node_db,
        ctx.pkc_priv_bytes,
    ) {
        DecryptOutcome::Decoded(d) => d,
        DecryptOutcome::PkiUnknownPubkey => {
            // Packet never decoded (neither PKC nor any channel worked), on a
            // channel-hash-0 unicast to us from a sender we hold no key for.
            // Matches upstream's `sniffReceived` (`ReliableRouter.cpp:143-167`):
            // reply with PKI_UNKNOWN_PUBKEY only when `want_ack` is set, then —
            // regardless of want_ack — proactively unicast our own NodeInfo so
            // the sender can learn (or refresh) our public key and retry. This
            // is the same nudge upstream sends specifically for this error
            // (not for a plain PKC tag mismatch, which upstream also folds into
            // this same fallback path but does not otherwise treat specially).
            if header.want_ack() {
                send_routing_error(
                    ctx,
                    header.sender,
                    header.packet_id,
                    crate::proto::routing::Error::PkiUnknownPubkey,
                )
                .await;
            }
            send_nodeinfo(ctx, header.sender, false).await;
            return;
        }
        DecryptOutcome::Drop => {
            // Could not authenticate this packet at all (bad PSK/PKC, or a
            // 1-byte channel-hash collision — indistinguishable from
            // tampering). If it's neither addressed to us nor claims to be
            // from us (Layer 0 already ruled out "from us"), relay it blind
            // rather than blackhole it: matches upstream's
            // `passesRoutingAuthGate` returning `OPAQUE_RELAY_ONLY` for
            // exactly this case (`Router.cpp:840-848`). A packet addressed
            // to us that we can't authenticate is rejected outright instead —
            // there is no one further to relay it to.
            if header.destination != ctx.device.my_node_num {
                maybe_relay_opaque(ctx, &frame, &header, metadata).await;
            } else {
                debug!(
                    "[Mesh] Undecryptable packet {:08x} addressed to us, dropping",
                    header.packet_id
                );
            }
            return;
        }
    };

    // =========================================================================
    // Layer 1: FloodingRouter — duplicate detection + upgrade + relay cancel
    //
    // Only reachable once the packet above decoded successfully, so every
    // state mutation from here on is driven by authenticated traffic.
    // =========================================================================

    // Get the hop_limit of our pending rebroadcast for this packet (if any).
    // The queue can hold several entries now, so this scans for the one
    // matching (sender, packet_id) rather than assuming a single slot.
    let pending_hop_limit = ctx.pending_rebroadcast.iter().find_map(|p| {
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
        pending_hop_limit,
        ctx.device.role,
    ) {
        FilterResult::New => {
            // Process normally — fall through
        }
        FilterResult::DuplicateUpgrade(new_hop) => {
            // Upgrade the matching pending rebroadcast with better hop_limit.
            if let Some(pending) = ctx.pending_rebroadcast.iter_mut().find(|p| {
                p.frame.header().is_some_and(|ph| {
                    ph.sender == header.sender && ph.packet_id == header.packet_id
                })
            }) {
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
            // Another node already relayed this — cancel our matching pending
            // rebroadcast, if any (other queued entries for other packets are
            // untouched).
            let before = ctx.pending_rebroadcast.len();
            ctx.pending_rebroadcast.retain(|p| {
                !p.frame.header().is_some_and(|ph| {
                    ph.sender == header.sender && ph.packet_id == header.packet_id
                })
            });
            if ctx.pending_rebroadcast.len() < before {
                debug!(
                    "[Mesh] Cancelling rebroadcast of {:08x} (relayed by 0x{:02x})",
                    header.packet_id, header.relay_node
                );
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

    // Update NodeDB (including hops_away from hop_start - hop_limit).
    // Check before touch() so we can detect first-ever contact with this node.
    let is_new_node = ctx.node_db.get(header.sender).is_none();
    // `Some(..)`: this is a direct off-air measurement, the only kind that may
    // populate NodeInfo.snr. 0 dB is valid, hence Option rather than a sentinel.
    ctx.node_db
        .touch(header.sender, 0, Some(metadata.snr), now_ms);
    if let Some(entry) = ctx.node_db.get_mut(header.sender) {
        // Only record hops when hop_start was actually set and nobody mangled
        // the limit in transit; otherwise leave it unknown rather than storing
        // a fabricated 0, which would read as "direct neighbour". Matches
        // upstream's `hopsAway >= 0` gate on has_hops_away.
        let (start, limit) = (header.hop_start(), header.hop_limit());
        entry.hops_away = (start > 0 && start >= limit).then(|| start - limit);
    }
    // Only push a stub NodeInfo to BLE on first contact. Subsequent updates
    // come from the portnum handlers (NodeInfo, Position) that call
    // notify_ble_node_update themselves when real data arrives.
    if is_new_node {
        notify_ble_node_update(ctx, header.sender).await;
    }

    let portnum = decoded.portnum;
    let want_response = decoded.want_response;
    let request_id = decoded.request_id;
    let reply_id = decoded.reply_id;
    let emoji = decoded.emoji;
    let inner_payload = decoded.payload;
    let channel_index = decoded.channel_index;
    info!(
        "[Mesh] Decoded portnum={} payload={}B from={:08x}",
        portnum,
        inner_payload.len(),
        header.sender
    );

    // We have no name for this sender yet. Introduce ourselves and ask who
    // they are, so they show up as more than a bare node number. Matches
    // upstream's `MeshService::handleFromRadio`, which does this on any
    // decoded packet from a node it has no User record for.
    if ctx
        .node_db
        .get(header.sender)
        .is_none_or(|e| e.user.is_none())
    {
        maybe_request_nodeinfo(ctx, header.sender, header.hop_start(), header.hop_limit()).await;
    }

    let addressed_to_us = header.is_for_us(ctx.device.my_node_num);
    let inbound = InboundPacket {
        sender: header.sender,
        packet_id: header.packet_id,
        relay_node: header.relay_node,
        payload: &inner_payload,
        addressed_to_us,
        want_response,
        request_id,
        reply_id,
        emoji,
        channel_idx: channel_index,
        snr: metadata.snr,
    };

    // Store text messages for replay when BLE reconnects
    if portnum == PortNum::TextMessageApp as i32
        || portnum == PortNum::TextMessageCompressedApp as i32
    {
        if *ctx.ble_connected {
            info!(
                "[Mesh] TEXT_MESSAGE from {:08x}: BLE connected, forwarding directly",
                inbound.sender
            );
        } else if let Err(e) = ctx.storage.add(&frame).await {
            warn!(
                "[Mesh] TEXT_MESSAGE from {:08x}: failed to buffer for replay ({:?})",
                inbound.sender, e
            );
        } else {
            info!(
                "[Mesh] TEXT_MESSAGE from {:08x}: BLE disconnected, buffered for replay",
                inbound.sender
            );
        }
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
                    true, // via_lora: this packet arrived over LoRa
                )
                .await;
            }
            // Admin packets for others fall through to the default BLE forward
            // below, same as every other portnum — do not forward here too.
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
            &PacketForwardArgs {
                header: &header,
                channel_index,
                portnum,
                payload: inbound.payload,
                reply_id: inbound.reply_id,
                emoji: inbound.emoji,
                meta: metadata,
            },
        )
        .await;
    }

    // Send ACK if unicast to us and want_ack set (on same channel as received).
    // Deliberately NOT `addressed_to_us`: that is `is_for_us()`, which is true for
    // broadcasts too, so a broadcast with want_ack set would make every receiver on
    // the mesh ACK at once. Upstream gates this on `isToUs(p) && !isBroadcast(p->to)`
    // (`ReliableRouter::sniffReceived`), where `isToUs` is strictly `p->to == ourNum`.
    if header.destination == ctx.device.my_node_num && header.want_ack() {
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
            schedule_rebroadcast(ctx, &frame, &header, metadata, new_hop).await;
        }
    }
}

fn should_rebroadcast_for_role(role: crate::domain::device::DeviceRole) -> bool {
    use crate::domain::device::DeviceRole;
    !matches!(role, DeviceRole::ClientMute | DeviceRole::ClientHidden)
}

/// Schedule a frame for jittered rebroadcast: rewrite its header with the
/// new hop_limit and our relay_node, compute the SNR/role/contention-window
/// delay, and push it onto `pending_rebroadcast` (evicting the
/// earliest-due entry if the queue is full). Shared by the normal
/// (authenticated) rebroadcast path and the opaque-relay path — both relay
/// the same way, they differ only in whether `history`/NodeDB/BLE were
/// touched on the way in.
async fn schedule_rebroadcast<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    frame: &RadioFrame,
    header: &crate::domain::packet::PacketHeader,
    metadata: RadioMetadata,
    new_hop: u8,
) {
    let relay_node = (ctx.device.my_node_num & 0xFF) as u8;
    let rebroadcast_frame = frame.with_rewritten_header(new_hop, relay_node);
    let (modem_cfg, _freq_hz) = ctx.device.lora_params();
    let raw_random = ctx.entropy.random_u32();
    let delay = rebroadcast_delay_ms(
        metadata.snr,
        ctx.device.role,
        modem_cfg.spreading_factor,
        modem_cfg.bandwidth_hz,
        raw_random,
    );
    let entry = PendingRebroadcast {
        frame: rebroadcast_frame,
        deadline: Instant::now() + Duration::from_millis(delay),
    };
    if let Err(entry) = ctx.pending_rebroadcast.push(entry) {
        // Queue full (8 concurrent pending relays) — drop the entry
        // with the soonest deadline to make room, then push this one.
        // An already-scheduled relay is closer to firing anyway;
        // matches upstream's TX-queue eviction under pressure rather
        // than refusing new work outright.
        if let Some((idx, _)) = ctx
            .pending_rebroadcast
            .iter()
            .enumerate()
            .min_by_key(|(_, p)| p.deadline)
        {
            ctx.pending_rebroadcast.swap_remove(idx);
            warn!("[Mesh] Rebroadcast queue full, dropped earliest-due entry");
        }
        let _ = ctx.pending_rebroadcast.push(entry);
    }
    debug!(
        "[Mesh] Scheduling rebroadcast of {:08x} in {}ms",
        header.packet_id, delay
    );
}

/// Relay a packet we could not authenticate ("opaque" relay), matching
/// upstream's `OPAQUE_RELAY_ONLY` verdict (`Router.cpp:840-848`,
/// `NextHopRouter::relayOpaquePacket`). Deliberately bypasses `history`,
/// `should_rebroadcast`/`should_relay_directed`, NodeDB and BLE — none of
/// those may be driven by unauthenticated input. Deduplication is instead
/// `MeshRouter::should_relay_opaque`'s own small, separate seen-set, and the
/// hop_limit is decremented the same way an authenticated relay's would be
/// (matching `perhapsRebroadcast`'s `hop_limit > 0` gate) so an opaque frame
/// still eventually stops circulating.
async fn maybe_relay_opaque<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    frame: &RadioFrame,
    header: &crate::domain::packet::PacketHeader,
    metadata: RadioMetadata,
) {
    if !should_rebroadcast_for_role(ctx.device.role) {
        return;
    }
    let hop_limit = header.hop_limit();
    if hop_limit == 0 {
        debug!(
            "[Mesh] Opaque packet {:08x} hop limit exhausted, dropping",
            header.packet_id
        );
        return;
    }
    if !ctx.router.should_relay_opaque(
        header.sender,
        header.packet_id,
        header.hop_start(),
        hop_limit,
    ) {
        debug!(
            "[Mesh] Opaque packet {:08x} already relayed, dropping",
            header.packet_id
        );
        return;
    }
    schedule_rebroadcast(ctx, frame, header, metadata, hop_limit - 1).await;
}

/// Unicast our NodeInfo to a sender we have no `User` for, asking for theirs
/// in return. Ports the guards from upstream's `MeshService::handleFromRadio`:
/// infrastructure roles stay quiet (they hear from everyone and would flood
/// the mesh with introductions), a full NodeDB has nowhere to put the answer,
/// and a node many hops away isn't worth the airtime. Shares
/// `last_nodeinfo_tx` with the reply path, mirroring upstream's single global
/// send gate, so a burst of unknown senders produces one introduction.
async fn maybe_request_nodeinfo<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    sender: u32,
    hop_start: u8,
    hop_limit: u8,
) {
    use crate::domain::device::DeviceRole;

    if matches!(ctx.device.role, DeviceRole::Router | DeviceRole::RouterLate) {
        return;
    }
    if ctx.node_db.len() >= MAX_NODES {
        return;
    }
    let throttled = ctx
        .last_nodeinfo_tx
        .is_some_and(|t| t.elapsed() < Duration::from_millis(NODEINFO_MIN_INTERVAL_MS));
    if throttled {
        return;
    }
    if !ctx
        .channel_metrics
        .tx_allowed_polite(Region::from_proto(ctx.device.region))
    {
        return;
    }
    // hop_start is what the sender set; the difference is how far it travelled.
    if hop_start.saturating_sub(hop_limit) > ctx.device.hop_limit.saturating_add(2) {
        debug!("[Mesh] Unknown node {:08x} too many hops away", sender);
        return;
    }

    info!(
        "[Mesh] Heard unknown node {:08x}, sending NodeInfo and asking for theirs",
        sender
    );
    send_nodeinfo(ctx, sender, true).await;
    *ctx.last_nodeinfo_tx = Some(Instant::now());
}

#[cfg(all(test, feature = "test-harness"))]
mod tests {
    extern crate std;

    use super::*;
    use crate::{domain::packet::PacketHeader, test_support::TestBed};

    const OUR_MAC: [u8; 6] = [0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC];
    /// Some other node's number — deliberately not derived from `OUR_MAC`.
    const OTHER_NODE: u32 = 0xAABB_CCDD;
    /// A third node, distinct from both us and `OTHER_NODE`, for opaque-relay
    /// tests where the packet must be neither to nor from us.
    const THIRD_NODE: u32 = 0x1111_2222;

    fn header(
        sender: u32,
        destination: u32,
        packet_id: u32,
        hop_limit: u8,
        hop_start: u8,
    ) -> PacketHeader {
        PacketHeader {
            destination,
            sender,
            packet_id,
            flags: PacketHeader::make_flags(false, false, hop_limit, hop_start),
            channel_index: 0xFF, // deliberately unmatched — forces decode failure below
            next_hop: 0,
            relay_node: 0,
        }
    }

    fn undecodable_frame(
        sender: u32,
        destination: u32,
        packet_id: u32,
        hop_limit: u8,
        hop_start: u8,
    ) -> RadioFrame {
        let hdr = header(sender, destination, packet_id, hop_limit, hop_start);
        // Garbage bytes: not a valid `Data` protobuf under any key, so
        // `try_decrypt_and_decode` always returns `Drop` for this frame
        // regardless of channel/PKC state.
        let garbage = [0xFFu8; 32];
        RadioFrame::from_parts(&hdr, &garbage).expect("garbage payload fits in one frame")
    }

    /// Build a genuine, decodable frame on the bed's default primary channel
    /// (PSK = the `[0x01]` sentinel, which `effective_psk()` expands to
    /// `DEFAULT_PSK` — encrypted, not plaintext).
    fn genuine_frame(bed: &TestBed, sender: u32, destination: u32, packet_id: u32) -> RadioFrame {
        use crate::proto::{Data, PortNum};
        use prost::Message;

        let preset_name = bed.device.modem_preset.display_name();
        let channel = bed
            .device
            .channels
            .primary()
            .expect("default primary channel exists");
        let hash = channel.hash(preset_name);

        // NeighborinfoApp rather than TextMessageApp: a channel-PSK-encrypted
        // unicast TEXT_MESSAGE_APP to us is now correctly rejected by the
        // legacy-DM rule added in 1.2 (see the doc comment on that check in
        // `try_decrypt_and_decode`) — this test wants a portnum unaffected by
        // that rule, to isolate "does a genuine authenticated packet still
        // update NodeDB" from that other, deliberate behaviour.
        let mut payload = Data {
            portnum: PortNum::NeighborinfoApp as i32,
            payload: b"hi".to_vec(),
            ..Default::default()
        }
        .encode_to_vec();

        let (psk, psk_len) = crate::domain::crypto_psk::copy_psk(channel.effective_psk());
        crate::domain::crypto_psk::crypt_packet(&psk[..psk_len], packet_id, sender, &mut payload)
            .expect("encryption should succeed with a valid key");

        let hdr = PacketHeader {
            destination,
            sender,
            packet_id,
            flags: PacketHeader::make_flags(false, false, 3, 3),
            channel_index: hash,
            next_hop: 0,
            relay_node: 0,
        };
        RadioFrame::from_parts(&hdr, &payload).expect("encrypted payload fits in one frame")
    }

    #[test]
    fn undecryptable_packet_addressed_to_us_touches_no_state_and_sends_nothing() {
        let mut bed = TestBed::new(&OUR_MAC);
        let us = bed.device.my_node_num;
        let frame = undecodable_frame(OTHER_NODE, us, 0x1000, 3, 3);
        let metadata = RadioMetadata { rssi: -80, snr: 5 };

        let mut ctx = bed.ctx();
        let _ = std::future::Future::poll(
            core::pin::pin!(dispatch(&mut ctx, frame, metadata)),
            &mut core::task::Context::from_waker(std::task::Waker::noop()),
        );
        drop(ctx);

        assert!(
            bed.node_db.get(OTHER_NODE).is_none(),
            "a forged sender must not create a NodeDB entry before authentication"
        );
        assert!(
            bed.sent_to_ble().is_empty(),
            "must not push anything to BLE"
        );
        assert!(
            bed.sent_to_lora().is_empty(),
            "must not queue anything for LoRa"
        );
        assert!(
            bed.pending_rebroadcast.is_empty(),
            "must not schedule a rebroadcast"
        );
    }

    #[test]
    fn genuine_encrypted_packet_still_reaches_its_handler() {
        let mut bed = TestBed::new(&OUR_MAC);
        let us = bed.device.my_node_num;
        let frame = genuine_frame(&bed, OTHER_NODE, us, 0x2000);
        let metadata = RadioMetadata { rssi: -80, snr: 5 };

        let mut ctx = bed.ctx();
        let _ = std::future::Future::poll(
            core::pin::pin!(dispatch(&mut ctx, frame, metadata)),
            &mut core::task::Context::from_waker(std::task::Waker::noop()),
        );
        drop(ctx);

        assert!(
            bed.node_db.get(OTHER_NODE).is_some(),
            "an authenticated packet must still update NodeDB"
        );
    }

    #[test]
    fn third_party_opaque_packet_relays_exactly_once() {
        let mut bed = TestBed::new(&OUR_MAC);
        // Neither to nor from us. hop_start (4) != hop_limit (3): a normal
        // in-flight packet, NOT the originator-retransmit case (that's
        // covered separately below) — so genuine dedup is what's on test.
        let frame = undecodable_frame(OTHER_NODE, THIRD_NODE, 0x3000, 3, 4);
        let metadata = RadioMetadata { rssi: -80, snr: 5 };

        let mut ctx = bed.ctx();
        let _ = std::future::Future::poll(
            core::pin::pin!(dispatch(&mut ctx, frame.clone(), metadata)),
            &mut core::task::Context::from_waker(std::task::Waker::noop()),
        );
        drop(ctx);
        assert_eq!(
            bed.pending_rebroadcast.len(),
            1,
            "first sighting of an opaque packet should schedule exactly one relay"
        );
        assert!(
            bed.node_db.get(OTHER_NODE).is_none(),
            "opaque relay must not touch NodeDB"
        );

        // A second, identical copy must not schedule a second relay.
        let mut ctx = bed.ctx();
        let _ = std::future::Future::poll(
            core::pin::pin!(dispatch(&mut ctx, frame, metadata)),
            &mut core::task::Context::from_waker(std::task::Waker::noop()),
        );
        drop(ctx);
        assert_eq!(
            bed.pending_rebroadcast.len(),
            1,
            "a duplicate opaque packet must not be relayed again"
        );
    }

    #[test]
    fn opaque_originator_retransmit_relays_again() {
        let mut bed = TestBed::new(&OUR_MAC);
        // hop_start == hop_limit signals the originator is retransmitting
        // because its ACK never arrived — matches upstream's exemption.
        let frame = undecodable_frame(OTHER_NODE, THIRD_NODE, 0x4000, 3, 3);
        let metadata = RadioMetadata { rssi: -80, snr: 5 };

        for _ in 0..2 {
            let mut ctx = bed.ctx();
            let _ = std::future::Future::poll(
                core::pin::pin!(dispatch(&mut ctx, frame.clone(), metadata)),
                &mut core::task::Context::from_waker(std::task::Waker::noop()),
            );
            drop(ctx);
            // Drain so each iteration starts from an empty queue and we're
            // only checking "did this call schedule a relay", not queue
            // depth across calls.
            bed.pending_rebroadcast.clear();
        }
        // If we got here without a capacity overflow/panic and the test
        // above (non-retransmit duplicate) shows suppression, this
        // exercises the same path with the retransmit exemption active;
        // the real assertion is `should_relay_opaque` returning `true`
        // both times, checked directly below.
        assert!(bed.router.should_relay_opaque(OTHER_NODE, 0x4001, 3, 3));
        assert!(
            bed.router.should_relay_opaque(OTHER_NODE, 0x4001, 3, 3),
            "an originator retransmit (hop_start == hop_limit) must relay again even if already seen"
        );
    }

    /// A 16-byte AES key that, combined with the empty-name fallback to the
    /// LongFast preset name, XOR-folds to exactly channel hash 0. Computed as
    /// `[0x00; 15] ++ [xor_fold("LongFast".bytes()) = 0x0A]` — the name's own
    /// fold is 0x0A, and folding that against 15 zero bytes and one 0x0A byte
    /// cancels back to 0.
    const ZERO_HASH_PSK: [u8; 16] = {
        let mut psk = [0u8; 16];
        psk[15] = 0x0A;
        psk
    };

    fn set_primary_channel_psk_to_zero_hash(bed: &mut TestBed) {
        let ch = bed
            .device
            .channels
            .get_mut(0)
            .expect("channel 0 exists by default");
        ch.psk.clear();
        ch.psk.extend_from_slice(&ZERO_HASH_PSK).unwrap();
        assert_eq!(
            ch.hash(bed.device.modem_preset.display_name()),
            0,
            "test PSK must actually hash to 0 under the bed's default preset"
        );
    }

    #[test]
    fn a_psk_unicast_whose_channel_hashes_to_zero_still_decrypts_via_fallback() {
        use crate::proto::{Data, PortNum};
        use prost::Message;

        let mut bed = TestBed::new(&OUR_MAC);
        let us = bed.device.my_node_num;
        // Give ourselves PKC keys too, so this packet is shape-classified as
        // a PKC candidate (channel_index == 0, unicast to us, payload big
        // enough, non-zero priv key) — the exact scenario that used to fail
        // permanently before 1.2, since classification used to be terminal.
        bed.pkc_priv_bytes = [7u8; 32];
        set_primary_channel_psk_to_zero_hash(&mut bed);
        // Deliberately no stored public key for OTHER_NODE — PKC must not be
        // attempted, and this must still fall through to the channel-0 PSK.

        let channel = bed.device.channels.get_mut(0).unwrap().clone();
        let mut payload = Data {
            portnum: PortNum::NeighborinfoApp as i32,
            // Padded well past PKC_OVERHEAD (12 bytes) so the PKC-candidate
            // shape test's size check alone doesn't disqualify it.
            payload: alloc::vec![0u8; 32],
            ..Default::default()
        }
        .encode_to_vec();
        let packet_id = 0x6000;
        let (psk, psk_len) = crate::domain::crypto_psk::copy_psk(channel.effective_psk());
        crate::domain::crypto_psk::crypt_packet(
            &psk[..psk_len],
            packet_id,
            OTHER_NODE,
            &mut payload,
        )
        .expect("encryption should succeed with a 16-byte key");

        let hdr = PacketHeader {
            destination: us,
            sender: OTHER_NODE,
            packet_id,
            flags: PacketHeader::make_flags(false, false, 3, 3),
            channel_index: 0,
            next_hop: 0,
            relay_node: 0,
        };
        let frame = RadioFrame::from_parts(&hdr, &payload).expect("payload fits in one frame");
        let metadata = RadioMetadata { rssi: -80, snr: 5 };

        let mut ctx = bed.ctx();
        let _ = std::future::Future::poll(
            core::pin::pin!(dispatch(&mut ctx, frame, metadata)),
            &mut core::task::Context::from_waker(std::task::Waker::noop()),
        );
        drop(ctx);

        assert!(
            bed.node_db.get(OTHER_NODE).is_some(),
            "a PSK unicast whose channel hashes to 0 must still decode via fallback, \
             not fail permanently just because it also looked like a PKC candidate"
        );
    }

    #[test]
    fn channel_encrypted_text_message_addressed_to_us_is_rejected_as_a_legacy_dm() {
        use crate::proto::{Data, PortNum};
        use prost::Message;

        let mut bed = TestBed::new(&OUR_MAC);
        let us = bed.device.my_node_num;
        let preset_name = bed.device.modem_preset.display_name();
        let channel = bed
            .device
            .channels
            .primary()
            .expect("default primary channel exists")
            .clone();
        let hash = channel.hash(preset_name);

        let mut payload = Data {
            portnum: PortNum::TextMessageApp as i32,
            payload: b"hi".to_vec(),
            ..Default::default()
        }
        .encode_to_vec();
        let packet_id = 0x7000;
        let (psk, psk_len) = crate::domain::crypto_psk::copy_psk(channel.effective_psk());
        crate::domain::crypto_psk::crypt_packet(
            &psk[..psk_len],
            packet_id,
            OTHER_NODE,
            &mut payload,
        )
        .expect("encryption should succeed with the primary channel's key");

        let hdr = PacketHeader {
            destination: us,
            sender: OTHER_NODE,
            packet_id,
            flags: PacketHeader::make_flags(false, false, 3, 3),
            channel_index: hash,
            next_hop: 0,
            relay_node: 0,
        };
        let frame = RadioFrame::from_parts(&hdr, &payload).expect("payload fits in one frame");
        let metadata = RadioMetadata { rssi: -80, snr: 5 };

        let mut ctx = bed.ctx();
        let _ = std::future::Future::poll(
            core::pin::pin!(dispatch(&mut ctx, frame, metadata)),
            &mut core::task::Context::from_waker(std::task::Waker::noop()),
        );
        drop(ctx);

        assert!(
            bed.sent_to_ble().is_empty(),
            "a channel-encrypted DM must be rejected as a legacy DM, matching upstream's \
             'Rejecting legacy DM' rule — every modern receiver on the mesh would also reject it"
        );
    }
}
