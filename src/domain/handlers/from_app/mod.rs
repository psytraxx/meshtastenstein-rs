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
            decode_psk_frame, make_from_radio_packet, next_from_radio_id, push_from_radio,
            send_ble_routing_ack,
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
            send_config_exchange(ctx, id).await;
            replay_stored_frames(ctx).await;
        }
        _ => {}
    }

    let _ = ctx
        .led_commands
        .try_send(LedCommand::Blink(LedPattern::DoubleBlink));
}

async fn transmit_from_ble_packet<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, pkt: MeshPacket) {
    let (portnum, inner_payload, request_id) = match &pkt.payload_variant {
        Some(mesh_packet::PayloadVariant::Decoded(data)) => {
            (data.portnum as u32, data.payload.clone(), data.request_id)
        }
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
            crate::domain::handlers::admin::dispatch(ctx, from, req_pkt_id, &inner_payload).await;
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
        let extra_nonce = esp_hal::rng::Rng::new().random();
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
        hop_limit,
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
        };
        if ctx.pending_packets.push(ack_entry).is_err() {
            warn!(
                "[Mesh] pending_packets full ({} entries), ACK tracking dropped for {:08x}",
                ctx.pending_packets.capacity(),
                packet_id
            );
        } else {
            info!("[Mesh] Tracking ACK for packet {:08x}", packet_id);
        }
    }
    ctx.tx_to_lora.send(frame).await;

    // Send local "sent" confirmation to the phone
    let ack_dest = if from == 0 {
        ctx.device.my_node_num
    } else {
        from
    };
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

    // 2. Our own NodeInfo
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

    // 3. DeviceMetadata
    push_from_radio(
        ctx,
        from_radio::PayloadVariant::Metadata(DeviceMetadata {
            firmware_version: FIRMWARE_VERSION.into(),
            device_state_version: DEVICE_STATE_VERSION,
            has_bluetooth: true,
            hw_model: ctx.device.hw_model as i32,
            ..Default::default()
        }),
    )
    .await;

    // 4. All 8 channels
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

    // 5. All Config types
    let lora_cfg = crate::domain::handlers::admin::build_lora_config(ctx.device);
    for variant in [
        config::PayloadVariant::Device(config::DeviceConfig::default()),
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
        config::PayloadVariant::Security(config::SecurityConfig::default()),
        config::PayloadVariant::Sessionkey(config::SessionkeyConfig {}),
    ] {
        push_from_radio(
            ctx,
            from_radio::PayloadVariant::Config(Config {
                payload_variant: Some(variant),
            }),
        )
        .await;
    }

    // 6. All ModuleConfig types
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
    ] {
        push_from_radio(
            ctx,
            from_radio::PayloadVariant::ModuleConfig(ModuleConfig {
                payload_variant: Some(variant),
            }),
        )
        .await;
    }

    // 7. NodeDB entries
    let mut node_nums: Vec<u32, 64> = Vec::new();
    for entry in ctx.node_db.iter() {
        node_nums.push(entry.node_num).ok();
    }
    for num in &node_nums {
        if let Some(entry) = ctx.node_db.get(*num) {
            let id = next_from_radio_id(ctx.from_radio_id);
            let data = crate::domain::handlers::util::make_node_info_from_radio(id, entry);
            ctx.tx_to_ble.send(FromRadioMessage { data, id }).await;
        }
    }

    // 8. ConfigCompleteId
    push_from_radio(ctx, from_radio::PayloadVariant::ConfigCompleteId(config_id)).await;

    info!("[Mesh] Config exchange complete, id={}", config_id);
}

async fn replay_stored_frames<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>) {
    let count = ctx.storage.count();
    if count == 0 {
        return;
    }
    info!("[Mesh] Replaying {} buffered frame(s) to BLE", count);
    while let Ok(Some(frame)) = ctx.storage.peek() {
        let _ = ctx.storage.pop();

        let header = match frame.header() {
            Some(h) => h,
            None => continue,
        };

        let Some((portnum, inner_payload, channel_index)) = decode_psk_frame(&frame, ctx.device)
        else {
            continue;
        };

        let id = next_from_radio_id(ctx.from_radio_id);
        let data = make_from_radio_packet(
            id,
            &header,
            channel_index,
            portnum,
            &inner_payload,
            RadioMetadata { snr: 0, rssi: 0 },
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
