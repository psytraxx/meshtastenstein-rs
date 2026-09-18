//! Dispatch for packets arriving from the BLE app (phone → device).
//!
//! `dispatch()` is async and performs side effects directly via `MeshCtx`.
//!
//! # How to add a new BLE → LoRa feature
//! 1. Add a portnum arm in `transmit_from_ble_packet` (or handle locally before the LoRa path)
//! 2. For local-only handling: process and `return` early (see `PortNum::AdminApp`)
//! 3. For LoRa forwarding: fall through to the encrypt + `ctx.tx_to_lora.send` path

pub mod position;

extern crate alloc;
use crate::{
    constants::*,
    domain::{
        context::MeshCtx,
        crypto_pkc::portnum_allows_pkc,
        handlers::util::{
            PacketForwardArgs, decode_psk_frame, make_from_radio_packet, next_from_radio_id,
            push_from_radio, send_ble_routing_ack, send_ble_routing_result, send_nodeinfo,
        },
        node_db::NodeDB,
        packet::{BROADCAST_ADDR, PortNumDisplay},
        router::PendingPacket,
        tx::TxBuilder,
    },
    inter_task::channels::{FromRadioMessage, LedCommand, LedPattern, RadioMetadata},
    ports::MeshStorage,
    proto::{
        Channel, ChannelSettings, Config, DeviceMetadata, MeshPacket, ModuleConfig, MyNodeInfo,
        PortNum, ToRadio, User, config, from_radio, mesh_packet, module_config, routing,
    },
};
use embassy_time::{Duration, Instant};
use heapless::Vec;
use log::{info, warn};
use prost::Message;

pub async fn dispatch<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, data: Vec<u8, 512>) {
    if data.is_empty() {
        return;
    }

    let to_radio = match ToRadio::decode(data.as_slice()) {
        Ok(t) => t,
        Err(e) => {
            warn!("[Mesh] ToRadio decode failed: {:?}", e);
            return;
        }
    };

    match to_radio.payload_variant {
        Some(crate::proto::to_radio::PayloadVariant::Packet(pkt)) => {
            transmit_from_ble_packet(ctx, pkt).await;
        }
        Some(crate::proto::to_radio::PayloadVariant::WantConfigId(id)) => {
            info!("[Mesh] Phone wants config, id={}", id);
            // The app's config download is a two-stage handshake keyed on
            // this nonce (see SPECIAL_NONCE_ONLY_NODES's doc comment): stage
            // 1 (any other id, in practice SPECIAL_NONCE_ONLY_CONFIG) wants
            // the full config; stage 2 wants only node info. The app has no
            // guard against receiving the full exchange during stage 2 — it
            // silently resets to stage 1 and discards what it already
            // collected — so replaying send_config_exchange here would hang
            // the app rather than complete the handshake.
            if id == SPECIAL_NONCE_ONLY_NODES {
                send_nodeinfo_exchange(ctx, id).await;
            } else {
                send_config_exchange(ctx, id).await;
            }
            replay_stored_frames(ctx).await;
        }
        Some(crate::proto::to_radio::PayloadVariant::Heartbeat(hb)) => {
            // The app's keepalive is a request/response pair: it writes a
            // heartbeat and waits for a QueueStatus back. Answering with
            // nothing leaves it stuck even after a complete config exchange,
            // because the handshake succeeds and then the liveness check
            // never does. Upstream replies to every heartbeat this way.
            //
            // nonce == 1 is upstream's "nodeinfo ping": re-broadcast our
            // NodeInfo so peers can re-learn our public key after a reboot,
            // instead of the plain keepalive reply.
            if hb.nonce == 1 {
                info!("[Mesh] Heartbeat nodeinfo ping — broadcasting NodeInfo");
                send_nodeinfo(ctx, BROADCAST_ADDR, false).await;
            } else {
                push_from_radio(
                    ctx,
                    from_radio::PayloadVariant::QueueStatus(crate::proto::QueueStatus {
                        res: 0,
                        free: ctx.tx_to_lora.free_capacity() as u32,
                        maxlen: LORA_TX_QUEUE_SIZE as u32,
                        mesh_packet_id: 0,
                    }),
                )
                .await;
            }
        }
        Some(crate::proto::to_radio::PayloadVariant::Disconnect(_)) => {
            // The phone is telling us it's about to drop the link (mirrors
            // upstream's PhoneAPI::close()). Signal the BLE task to end the
            // connection now rather than waiting for the ATT-level teardown —
            // this closes the window where the queue could still be pushed
            // to for a connection the phone has already moved on from. The
            // BLE task drains any leftover queued messages before accepting
            // the next connection regardless, so this only removes the delay.
            let _ = ctx.disconn_cmd.try_send(());
        }
        _ => {}
    }

    let _ = ctx
        .led_commands
        .try_send(LedCommand::Blink(LedPattern::DoubleBlink));
}

async fn transmit_from_ble_packet<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, pkt: MeshPacket) {
    let (portnum, inner_payload, request_id, reply_id, emoji) = match &pkt.payload_variant {
        Some(mesh_packet::PayloadVariant::Decoded(data)) => (
            data.portnum as u32,
            data.payload.clone(),
            data.request_id,
            data.reply_id,
            data.emoji,
        ),
        _ => {
            warn!("[Mesh] Non-decoded packet from BLE, ignoring");
            return;
        }
    };

    let to = pkt.to;
    let from = pkt.from;
    let req_pkt_id = pkt.id;

    // Single chokepoint for every outgoing packet the phone hands us over
    // BLE, mirroring from_radio::dispatch's RX log — makes it possible to
    // see, by eye, what the app actually asked for (channel index in
    // particular: PKC_CHANNEL_INDEX (8) vs. a real channel) before this
    // function decides how to route it.
    info!(
        "[Mesh] BLE ToRadio: from={:08x} to={:08x} id={:08x} ch=0x{:02x} portnum={} want_ack={}",
        from,
        to,
        req_pkt_id,
        pkt.channel,
        PortNumDisplay(portnum as i32),
        pkt.want_ack,
    );

    // Handle special portnums locally or gate them
    match PortNum::try_from(portnum as i32).ok() {
        Some(PortNum::PositionApp) => {
            // M6: Save position payload for periodic re-broadcast
            ctx.my_position_bytes.clear();
            ctx.my_position_bytes.extend_from_slice(&inner_payload).ok();
        }
        Some(PortNum::AdminApp)
            if to == ctx.device.my_node_num || to == BROADCAST_ADDR || to == 0 =>
        {
            crate::domain::handlers::admin::dispatch(
                ctx,
                from,
                req_pkt_id,
                &inner_payload,
                false, // via_lora: this packet arrived over BLE
            )
            .await;
            // Send routing ACK so the app knows the admin message was received.
            if pkt.want_ack {
                send_ble_routing_ack(ctx, from, req_pkt_id).await;
            }
            return;
        }
        _ => {}
    }

    let packet_id = if pkt.id != 0 {
        pkt.id
    } else {
        ctx.device.next_packet_id()
    };
    let hop_limit = (pkt.hop_limit as u8).min(MAX_HOP_LIMIT);
    // Text messages auto-set want_ack
    let want_ack = pkt.want_ack || portnum == PortNum::TextMessageApp as u32;

    // Decide PKC vs. channel PSK from what the phone actually asked for, not
    // from a heuristic. The Android app sends channel index `PKC_CHANNEL_INDEX`
    // (8, one past the last real channel) to explicitly request PKC — it is
    // never a real `ChannelSet` index and must never be passed through to
    // `TxBuilder`, which would otherwise silently fall back to the primary
    // channel's PSK (`ChannelSet::get` returns `None` for an out-of-range
    // index, and `TxBuilder::build` falls back to `channels.primary()` on
    // `None`). That silent downgrade is exactly the bug this replaces:
    // upstream's `perhapsEncode` refuses outright (`PKI_SEND_FAIL_PUBLIC_KEY`)
    // rather than falling back to a legacy channel-encrypted DM, because
    // receivers on 2.5.0+ reject a channel-encrypted TEXT_MESSAGE_APP
    // addressed to them as a "legacy DM" (`Router.cpp:1057-1059`) — a silent
    // PSK fallback here would produce a message the recipient's own firmware
    // throws away, while this device reports success.
    let wants_pkc = pkt.channel == u32::from(PKC_CHANNEL_INDEX);
    let channel_idx = if wants_pkc { 0 } else { pkt.channel as u8 };

    if channel_idx >= MAX_CHANNELS as u8 && !wants_pkc {
        warn!(
            "[Mesh] BLE->LoRa: out-of-range channel {} for {:08x}, dropping",
            pkt.channel, to
        );
        return;
    }

    let pkc_keys = if wants_pkc {
        // Portnums that must stay under the channel PSK even when the phone
        // asked for PKC (traceroute/NodeInfo/routing/position — see
        // `portnum_allows_pkc`'s doc comment). Upstream's `wouldEncryptWithPKC`
        // applies this exclusion regardless of what the caller requested, so
        // do the same rather than trusting the phone's `channel` field blindly.
        if !portnum_allows_pkc(portnum as i32) {
            warn!(
                "[Mesh] BLE->LoRa: portnum {} may not use PKC, dropping",
                PortNumDisplay(portnum as i32)
            );
            return;
        }
        if to == BROADCAST_ADDR || to == 0 {
            // PKC is meaningless for a broadcast; upstream's
            // `wouldEncryptWithPKC` excludes `isBroadcast(p->to)` outright.
            warn!("[Mesh] BLE->LoRa: PKC requested for a broadcast, dropping");
            return;
        }
        // A client-supplied key must match what we have on file — upstream
        // treats a mismatch as `PKI_FAILED` (`Router.cpp:1294-1298`) rather
        // than trusting the phone's copy or silently overwriting ours.
        if pkt.public_key.len() == 32
            && ctx
                .node_db
                .get(to)
                .and_then(|e| e.pub_key)
                .is_some_and(|stored| stored != pkt.public_key.as_slice())
        {
            warn!(
                "[Mesh] BLE->LoRa: phone's public key for {:08x} does not match stored key, dropping",
                to
            );
            send_ble_routing_result(ctx, to, req_pkt_id, routing::Error::PkiFailed).await;
            return;
        }
        if !NodeDB::has_pub_key(ctx.node_db, to) || !ctx.pkc_priv_bytes.iter().any(|&b| b != 0) {
            // No known key for this destination (or we have no keypair of
            // our own yet) — refuse outright. There is deliberately no PSK
            // fallback here: see the comment above `wants_pkc`.
            warn!(
                "[Mesh] BLE->LoRa: no public key for {:08x}, refusing PKC DM",
                to
            );
            send_ble_routing_result(ctx, to, req_pkt_id, routing::Error::PkiSendFailPublicKey)
                .await;
            return;
        }
        let extra_nonce = ctx.entropy.random_u32();
        Some((ctx.pkc_priv_bytes as &[u8; 32], extra_nonce))
    } else {
        None
    };

    let frame = TxBuilder {
        dest: to,
        portnum: portnum as i32,
        inner_payload,
        channel_idx: Some(channel_idx),
        want_ack,
        request_id,
        reply_id,
        emoji,
        hop_limit: Some(hop_limit),
        ..Default::default()
    }
    .build(ctx.device, ctx.router, ctx.node_db, packet_id, pkc_keys);

    let Some(frame) = frame else {
        warn!("[Mesh] BLE->LoRa: frame build failed for {:08x}", to);
        return;
    };

    let next_hop = frame.header().map(|h| h.next_hop).unwrap_or(0);
    info!(
        "[Mesh] BLE->LoRa: portnum={} to={:08x} next_hop=0x{:02x}",
        PortNumDisplay(portnum as i32),
        to,
        next_hop
    );

    let ack_dest = if from == 0 {
        ctx.device.my_node_num
    } else {
        from
    };

    let is_broadcast = to == BROADCAST_ADDR;
    let ota_want_ack = want_ack && !is_broadcast;
    if ota_want_ack {
        let ack_entry = PendingPacket {
            frame: frame.clone(),
            packet_id,
            dest: to,
            sender: ctx.device.my_node_num,
            deadline: Instant::now() + Duration::from_millis(WANT_ACK_TIMEOUT_MS),
            retries_left: NUM_RELIABLE_RETX,
            is_our_packet: true,
            ble_notify: Some(ack_dest),
        };
        if ctx.pending_packets.push(ack_entry).is_err() {
            warn!(
                "[Mesh] pending_packets full ({} entries), ACK tracking dropped for {:08x}",
                ctx.pending_packets.capacity(),
                packet_id
            );
            // Tracking failed — there's no path left to report a real outcome,
            // so fall back to the old best-effort "sent" confirmation rather
            // than leaving the phone waiting forever.
            send_ble_routing_ack(ctx, ack_dest, req_pkt_id).await;
        } else {
            info!("[Mesh] Tracking ACK for packet {:08x}", packet_id);
        }
        ctx.tx_to_lora.send(frame).await;
        // Deliberately no immediate BLE confirmation here: the real mesh
        // outcome is reported later, by `from_radio::routing::handle` on a
        // genuine ACK or by `tick_retransmissions`'s giving-up path on
        // exhaustion — see `send_ble_routing_result`. Sending a "delivered"
        // confirmation here, before the mesh had even attempted delivery,
        // was the actual bug.
        return;
    }

    ctx.tx_to_lora.send(frame).await;

    // No mesh ACK was requested (or this was a broadcast) — there's nothing
    // further to report, so the local "sent" confirmation is the whole story.
    send_ble_routing_ack(ctx, ack_dest, req_pkt_id).await;
}

async fn send_config_exchange<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, config_id: u32) {
    let my_num = ctx.device.my_node_num;
    let nodedb_count = 1u32 + ctx.node_db.len() as u32;

    // 1. MyNodeInfo
    push_from_radio(
        ctx,
        from_radio::PayloadVariant::MyInfo(MyNodeInfo {
            my_node_num: my_num,
            nodedb_count,
            min_app_version: MIN_APP_VERSION,
            ..Default::default()
        }),
    )
    .await;

    // 2. DeviceUIConfig. Upstream sends this immediately after MyNodeInfo
    // (`STATE_SEND_MY_INFO` → `STATE_SEND_UIDATA` → `STATE_SEND_OWN_NODEINFO`)
    // and the app's handshake state machine expects it in that slot. It's a
    // UI-preferences blob for on-device TFT screens, so a default one is the
    // honest answer for a board with no display — but omitting the message
    // entirely left the app waiting for a step that never came.
    push_from_radio(
        ctx,
        from_radio::PayloadVariant::DeviceuiConfig(crate::proto::DeviceUiConfig::default()),
    )
    .await;

    // 3. Our own NodeInfo
    push_own_node_info(ctx).await;

    // 4. DeviceMetadata
    push_from_radio(
        ctx,
        from_radio::PayloadVariant::Metadata(DeviceMetadata {
            firmware_version: FIRMWARE_VERSION.into(),
            device_state_version: DEVICE_STATE_VERSION,
            has_bluetooth: true,
            hw_model: ctx.device.hw_model as i32,
            role: ctx.device.role as i32,
            ..Default::default()
        }),
    )
    .await;

    // 5. Region → legal modem presets. Upstream sends this between the metadata
    // and the channels, and the app expects one message in that slot. We have no
    // region/preset constraint table of our own, so the map goes out empty: the
    // app then leaves its region and preset pickers unconstrained rather than
    // waiting for a step that never arrives.
    push_from_radio(
        ctx,
        from_radio::PayloadVariant::RegionPresets(crate::proto::LoRaRegionPresetMap::default()),
    )
    .await;

    // 6. All 8 channels
    for idx in 0u8..8u8 {
        let ch_msg = if let Some(ch) = ctx.device.channels.get(idx) {
            Channel {
                index: idx as i32,
                settings: Some(ChannelSettings {
                    psk: ch.psk.to_vec(),
                    name: ch.name.as_str().into(),
                    ..Default::default()
                }),
                role: ch.role as i32,
            }
        } else {
            Channel {
                index: idx as i32,
                settings: None,
                role: 0,
            }
        };
        push_from_radio(ctx, from_radio::PayloadVariant::Channel(ch_msg)).await;
    }

    // 7. All Config types
    let lora_cfg = crate::domain::handlers::admin::build_lora_config(ctx.device);
    for variant in [
        // `role` populated to match what an explicit GetConfigRequest(DeviceConfig)
        // already returns — previously this sent `::default()`, so the phone's
        // initial handshake always showed role Client regardless of the real
        // configured role, and only a later explicit GetConfig corrected it.
        config::PayloadVariant::Device(config::DeviceConfig {
            role: ctx.device.role as i32,
            ..Default::default()
        }),
        config::PayloadVariant::Position(config::PositionConfig::default()),
        config::PayloadVariant::Power(config::PowerConfig::default()),
        config::PayloadVariant::Network(config::NetworkConfig::default()),
        config::PayloadVariant::Display(config::DisplayConfig::default()),
        config::PayloadVariant::Lora(lora_cfg),
        config::PayloadVariant::Bluetooth(config::BluetoothConfig {
            enabled: true,
            mode: config::bluetooth_config::PairingMode::RandomPin as i32,
            ..Default::default()
        }),
        // Upstream hands an admin-authorized client the whole `config.security`
        // struct, private key included (`PhoneAPI.cpp:781-798`); only an
        // *unauthenticated* client gets a zeroed one. A BLE-paired phone is on
        // the authorized side of that split, and it needs the private key to
        // encrypt direct messages itself — with only the public half it treats
        // the device as having no keypair, never requests PKC, and silently
        // sends every DM under the channel PSK for the recipient to reject.
        config::PayloadVariant::Security(config::SecurityConfig {
            public_key: ctx.pkc_pub_bytes.to_vec(),
            private_key: ctx.pkc_priv_bytes.to_vec(),
            ..Default::default()
        }),
        config::PayloadVariant::Sessionkey(config::SessionkeyConfig {}),
        // Upstream sends this as a no-op payload — the real UI blob already went
        // out as its own FromRadio message earlier. The app still counts one
        // Config message per ConfigType, so skipping it leaves it waiting.
        config::PayloadVariant::DeviceUi(crate::proto::DeviceUiConfig::default()),
    ] {
        push_from_radio(
            ctx,
            from_radio::PayloadVariant::Config(Config {
                payload_variant: Some(variant),
            }),
        )
        .await;
    }

    // 8. All ModuleConfig types
    for variant in [
        module_config::PayloadVariant::Mqtt(module_config::MqttConfig::default()),
        module_config::PayloadVariant::Serial(module_config::SerialConfig::default()),
        module_config::PayloadVariant::ExternalNotification(
            module_config::ExternalNotificationConfig::default(),
        ),
        module_config::PayloadVariant::StoreForward(module_config::StoreForwardConfig::default()),
        module_config::PayloadVariant::RangeTest(module_config::RangeTestConfig::default()),
        module_config::PayloadVariant::Telemetry(module_config::TelemetryConfig::default()),
        module_config::PayloadVariant::CannedMessage(module_config::CannedMessageConfig::default()),
        module_config::PayloadVariant::Audio(module_config::AudioConfig::default()),
        module_config::PayloadVariant::RemoteHardware(
            module_config::RemoteHardwareConfig::default(),
        ),
        module_config::PayloadVariant::NeighborInfo(module_config::NeighborInfoConfig::default()),
        module_config::PayloadVariant::AmbientLighting(
            module_config::AmbientLightingConfig::default(),
        ),
        module_config::PayloadVariant::DetectionSensor(
            module_config::DetectionSensorConfig::default(),
        ),
        module_config::PayloadVariant::Paxcounter(module_config::PaxcounterConfig::default()),
        module_config::PayloadVariant::Statusmessage(module_config::StatusMessageConfig::default()),
        module_config::PayloadVariant::TrafficManagement(
            module_config::TrafficManagementConfig::default(),
        ),
        module_config::PayloadVariant::Tak(module_config::TakConfig::default()),
        module_config::PayloadVariant::MeshBeacon(module_config::MeshBeaconConfig::default()),
    ] {
        push_from_radio(
            ctx,
            from_radio::PayloadVariant::ModuleConfig(ModuleConfig {
                payload_variant: Some(variant),
            }),
        )
        .await;
    }

    // 9. ConfigCompleteId. Stage 1 (this exchange) skips the NodeDB entirely,
    // matching upstream's SPECIAL_NONCE_ONLY_CONFIG behavior — it goes out
    // only in send_nodeinfo_exchange's stage 2.
    push_from_radio(ctx, from_radio::PayloadVariant::ConfigCompleteId(config_id)).await;

    info!("[Mesh] Config exchange complete, id={}", config_id);
}

/// Stage 2 of the app's config-download handshake (`SPECIAL_NONCE_ONLY_NODES`):
/// own NodeInfo, then the NodeDB, then ConfigCompleteId. No MyNodeInfo,
/// DeviceUIConfig, DeviceMetadata, RegionPresets, Channels, Config or
/// ModuleConfig — see that constant's doc comment for why sending those here
/// hangs the app instead of completing the handshake.
async fn send_nodeinfo_exchange<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, config_id: u32) {
    push_own_node_info(ctx).await;
    push_node_db(ctx).await;
    push_from_radio(ctx, from_radio::PayloadVariant::ConfigCompleteId(config_id)).await;

    // Peer battery/position arrive *after* ConfigCompleteId as synthesised
    // broadcast packets, not bundled into the NodeInfos above — upstream sends
    // other-node NodeInfo thin and drains its satellite DBs here, interleaved
    // with live traffic in STATE_SEND_PACKETS.
    crate::domain::handlers::util::replay_satellite_db(ctx).await;

    info!("[Mesh] Node info exchange complete, id={}", config_id);
}

/// Pushes the device's own NodeInfo as a FromRadio message. Sent by both
/// handshake stages — upstream's stage 2 also starts at `STATE_SEND_OWN_NODEINFO`.
async fn push_own_node_info<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>) {
    let my_num = ctx.device.my_node_num;
    push_from_radio(
        ctx,
        from_radio::PayloadVariant::NodeInfo(crate::proto::NodeInfo {
            num: my_num,
            user: Some(User {
                id: ctx.node_id_str.into(),
                long_name: ctx.device.long_name.as_str().into(),
                short_name: ctx.device.short_name.as_str().into(),
                hw_model: ctx.device.hw_model as i32,
                is_licensed: false,
                // The app reads this device's own key from here, not just from
                // `config.security`. Omitting it left both key fields blank in
                // the app, so it believed the device had no keypair and could
                // not offer PKC to any contact — every DM silently downgraded
                // to a channel-PSK message the recipient then rejected. The
                // LoRa broadcast of our NodeInfo has always carried it.
                public_key: ctx.pkc_pub_bytes.to_vec(),
                ..Default::default()
            }),
            is_favorite: true,
            ..Default::default()
        }),
    )
    .await;
}

/// Pushes up to `CONFIG_EXCHANGE_MAX_NODES` NodeDB entries as FromRadio
/// NodeInfo messages — see that constant's doc for why the full DB can't be
/// streamed here without dropping ConfigCompleteId. The phone picks up the
/// rest from live traffic once connected.
async fn push_node_db<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>) {
    let mut node_nums: Vec<u32, CONFIG_EXCHANGE_MAX_NODES> = Vec::new();
    for entry in ctx.node_db.iter() {
        if node_nums.push(entry.node_num).is_err() {
            break;
        }
    }
    info!(
        "[Diag] push_node_db: our_num={:08x} emitting {} entries: {:08x?}",
        ctx.device.my_node_num,
        node_nums.len(),
        node_nums.as_slice()
    );
    for num in &node_nums {
        if let Some(entry) = ctx.node_db.get(*num) {
            if *num == ctx.device.my_node_num {
                warn!(
                    "[Diag] push_node_db is re-sending OUR OWN node {:08x} (pub_key={}, user={})",
                    num,
                    entry.pub_key.is_some(),
                    entry.user.is_some()
                );
            }
            let id = next_from_radio_id(ctx.from_radio_id);
            let data = crate::domain::handlers::util::make_node_info_from_radio(id, entry);
            // try_send, not send: see `push_from_radio` — awaiting here would
            // stall the whole orchestrator if the phone drops mid-exchange.
            if ctx
                .tx_to_ble
                .try_send(FromRadioMessage { data, id })
                .is_err()
            {
                warn!("[Mesh] BLE TX queue full, dropped NodeInfo id={}", id);
            }
        }
    }
}

async fn replay_stored_frames<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>) {
    let count = ctx.storage.count();
    if count == 0 {
        return;
    }
    info!("[Mesh] Replaying {} buffered frame(s) to BLE", count);
    while let Ok(Some(frame)) = ctx.storage.peek().await {
        let _ = ctx.storage.pop().await;

        let header = match frame.header() {
            Some(h) => h,
            None => continue,
        };

        let Some(decoded) = decode_psk_frame(&frame, ctx.device) else {
            continue;
        };

        let id = next_from_radio_id(ctx.from_radio_id);
        let data = make_from_radio_packet(
            id,
            &PacketForwardArgs {
                header: &header,
                channel_index: decoded.channel_index,
                portnum: decoded.portnum,
                payload: decoded.payload.as_slice(),
                reply_id: decoded.reply_id,
                emoji: decoded.emoji,
                meta: RadioMetadata { snr: 0, rssi: 0 },
            },
        );
        if ctx
            .tx_to_ble
            .try_send(FromRadioMessage { data, id })
            .is_err()
        {
            warn!("[Mesh] BLE TX queue full, dropped stored frame id={}", id);
        }
    }
    info!("[Mesh] Store-and-forward replay complete");
}

#[cfg(all(test, feature = "test-harness"))]
mod tests {
    extern crate std;

    use super::*;
    use crate::{
        proto::{Data, PortNum},
        test_support::TestBed,
    };

    const OUR_MAC: [u8; 6] = [0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC];
    const OTHER_NODE: u32 = 0xAABB_CCDD;

    fn ble_text_packet(to: u32, channel: u32) -> MeshPacket {
        MeshPacket {
            to,
            from: 0,
            id: 0x5000,
            channel,
            want_ack: false,
            payload_variant: Some(mesh_packet::PayloadVariant::Decoded(Data {
                portnum: PortNum::TextMessageApp as i32,
                payload: b"hi".to_vec(),
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    /// Drive `transmit_from_ble_packet` to completion. There's no executor in
    /// this harness, but every branch these tests exercise completes
    /// synchronously against the fakes (`FakeStorage`, `FakeEntropy`), so a
    /// single poll is sufficient — a future still pending after one poll
    /// would mean the harness needs a real executor, which is worth knowing
    /// about rather than silently ignoring, hence the assert.
    fn run_transmit(ctx: &mut MeshCtx<'_, crate::test_support::FakeStorage>, pkt: MeshPacket) {
        let poll = std::future::Future::poll(
            core::pin::pin!(transmit_from_ble_packet(ctx, pkt)),
            &mut core::task::Context::from_waker(std::task::Waker::noop()),
        );
        assert!(
            poll.is_ready(),
            "transmit_from_ble_packet did not complete synchronously against the fakes"
        );
    }

    #[test]
    fn dm_to_a_keyless_node_refuses_rather_than_downgrading_to_psk() {
        let mut bed = TestBed::new(&OUR_MAC);
        // Give ourselves a real (non-zero) keypair — otherwise PKC is never
        // attempted regardless of what the phone asks for.
        bed.pkc_priv_bytes = [1u8; 32];
        // OTHER_NODE deliberately has no stored public key.

        let pkt = ble_text_packet(OTHER_NODE, u32::from(PKC_CHANNEL_INDEX));
        let mut ctx = bed.ctx();
        run_transmit(&mut ctx, pkt);
        drop(ctx);

        assert!(
            bed.sent_to_lora().is_empty(),
            "a keyless PKC DM must never be silently downgraded to a PSK-encrypted send"
        );
    }

    #[test]
    fn channel_8_takes_the_pkc_path_when_a_key_is_known() {
        let mut bed = TestBed::new(&OUR_MAC);
        bed.pkc_priv_bytes = [1u8; 32];
        bed.node_db.touch(OTHER_NODE, 0, None, 0);
        bed.node_db.update_pub_key(OTHER_NODE, [2u8; 32]);

        let pkt = ble_text_packet(OTHER_NODE, u32::from(PKC_CHANNEL_INDEX));
        let mut ctx = bed.ctx();
        run_transmit(&mut ctx, pkt);
        drop(ctx);

        let sent = bed.sent_to_lora();
        assert_eq!(sent.len(), 1, "expected exactly one frame queued for LoRa");
        let header = sent[0].header().expect("frame has a valid header");
        assert_eq!(
            header.channel_index, 0,
            "a PKC frame must carry channel_hash == 0 on the wire, never the primary channel's real hash"
        );
    }

    #[test]
    fn out_of_range_channel_index_is_rejected_not_mapped_to_primary() {
        let mut bed = TestBed::new(&OUR_MAC);
        // 9 is neither a real channel (0..8) nor the PKC sentinel (8).
        let pkt = ble_text_packet(OTHER_NODE, 9);
        let mut ctx = bed.ctx();
        run_transmit(&mut ctx, pkt);
        drop(ctx);

        assert!(
            bed.sent_to_lora().is_empty(),
            "an out-of-range channel index must be dropped, not silently sent on the primary channel"
        );
    }

    #[test]
    fn valid_channel_index_still_sends_normally() {
        let mut bed = TestBed::new(&OUR_MAC);
        let pkt = ble_text_packet(OTHER_NODE, 0);
        let mut ctx = bed.ctx();
        run_transmit(&mut ctx, pkt);
        drop(ctx);

        assert_eq!(
            bed.sent_to_lora().len(),
            1,
            "a normal, in-range channel index must still transmit"
        );
    }

    #[test]
    fn pkc_is_never_used_for_a_broadcast() {
        let mut bed = TestBed::new(&OUR_MAC);
        bed.pkc_priv_bytes = [1u8; 32];

        let pkt = ble_text_packet(
            crate::domain::packet::BROADCAST_ADDR,
            u32::from(PKC_CHANNEL_INDEX),
        );
        let mut ctx = bed.ctx();
        run_transmit(&mut ctx, pkt);
        drop(ctx);

        assert!(
            bed.sent_to_lora().is_empty(),
            "PKC requested for a broadcast destination must be refused, matching upstream's isBroadcast exclusion"
        );
    }
}
