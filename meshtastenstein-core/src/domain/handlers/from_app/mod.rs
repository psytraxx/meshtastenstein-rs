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
        handlers::util::{
            PacketForwardArgs, decode_psk_frame, make_from_radio_packet, next_from_radio_id,
            push_from_radio, send_ble_routing_ack, send_nodeinfo,
        },
        node_db::NodeDB,
        packet::BROADCAST_ADDR,
        router::PendingPacket,
        tx::TxBuilder,
    },
    inter_task::channels::{FromRadioMessage, LedCommand, LedPattern, RadioMetadata},
    ports::MeshStorage,
    proto::{
        Channel, ChannelSettings, Config, DeviceMetadata, MeshPacket, ModuleConfig, MyNodeInfo,
        PortNum, ToRadio, User, config, from_radio, mesh_packet, module_config,
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
    let channel_idx = pkt.channel as u8;

    // PKC when destination is unicast and has a stored public key in NodeDB.
    let pkc_keys = if to != BROADCAST_ADDR
        && to != 0
        && NodeDB::has_pub_key(ctx.node_db, to)
        && ctx.pkc_priv_bytes.iter().any(|&b| b != 0)
    {
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
        portnum, to, next_hop
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
        // The phone needs the device's X25519 public key here — it's what the
        // app shows in the device's QR/URL and what other nodes use to derive
        // the PKC shared secret. Upstream sends the full `config.security`
        // (including `public_key`) to a BLE-connected client, which is
        // admin-authorized by virtue of the pairing. An empty SecurityConfig
        // here left the app without the device's key. `private_key` stays out
        // — the app doesn't need it and it never leaves the device.
        config::PayloadVariant::Security(config::SecurityConfig {
            public_key: ctx.pkc_pub_bytes.to_vec(),
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
    for num in &node_nums {
        if let Some(entry) = ctx.node_db.get(*num) {
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
