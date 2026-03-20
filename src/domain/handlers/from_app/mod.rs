//! Dispatch for packets arriving from the BLE app (phone → device).
//!
//! `dispatch()` is async and performs side effects directly via `MeshCtx`.
//!
//! # How to add a new BLE → LoRa feature
//! 1. Add a portnum arm in `transmit_from_ble_packet` (or handle locally before the LoRa path)
//! 2. For local-only handling: process and `return` early (see `PortNum::AdminApp`)
//! 3. For LoRa forwarding: fall through to the encrypt + `ctx.tx_to_lora.send` path

pub mod position;

use crate::constants::*;
use crate::domain::context::MeshCtx;
use crate::domain::crypto;
use crate::domain::handlers::util::{
    encode_from_radio, make_from_radio_packet, next_from_radio_id, send_ble_routing_ack,
};
use crate::domain::packet::{PacketHeader, RadioFrame};
use crate::inter_task::channels::{
    FromRadioMessage, LedCommand, LedPattern, RadioMetadata, ToRadioMessage,
};
use crate::ports::MeshStorage;
use crate::proto::{
    Channel, ChannelSettings, Config, Data, DeviceMetadata, MeshPacket, ModuleConfig, MyNodeInfo,
    PortNum, ToRadio, User, config, from_radio, mesh_packet, module_config,
};
use embassy_time::{Duration, Instant};
use log::{info, warn};
use prost::Message;

pub async fn dispatch<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, msg: ToRadioMessage) {
    let data = msg.data.as_slice();
    if data.is_empty() {
        return;
    }

    let to_radio = match ToRadio::decode(data) {
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
        Some(PortNum::AdminApp) => {
            if to == ctx.device.my_node_num || to == 0xFFFF_FFFF || to == 0 {
                crate::domain::handlers::admin::dispatch(ctx, from, req_pkt_id, &inner_payload)
                    .await;
                // Send routing ACK so the app knows the admin message was received.
                if pkt.want_ack {
                    send_ble_routing_ack(ctx, from, req_pkt_id).await;
                }
                return;
            }
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

    // Encode Data payload with prost
    let mut enc_buf = Data {
        portnum: portnum as i32,
        payload: inner_payload,
        request_id,
        ..Default::default()
    }
    .encode_to_vec();

    // Get channel hash and optional PSK for encryption
    let preset_name = ctx.device.modem_preset.display_name();
    let channel = ctx.device.channels.get(channel_idx);
    let channel_hash = channel
        .or_else(|| ctx.device.channels.primary())
        .map(|c| c.hash(preset_name))
        .unwrap_or(0);

    let psk_for_encrypt = channel
        .or_else(|| ctx.device.channels.primary())
        .filter(|c| c.is_encrypted())
        .map(|c| crypto::copy_psk(c.effective_psk()));

    if let Some((psk_buf, psk_len)) = psk_for_encrypt {
        let _ = crypto::crypt_packet(
            &psk_buf[..psk_len],
            packet_id,
            ctx.device.my_node_num,
            &mut enc_buf,
        );
    }

    // Broadcast packets don't get mesh-level ACKs
    let is_broadcast = to == 0xFFFF_FFFF;
    let ota_want_ack = want_ack && !is_broadcast;

    let header = PacketHeader {
        destination: to,
        sender: ctx.device.my_node_num,
        packet_id,
        flags: PacketHeader::make_flags(ota_want_ack, false, hop_limit, hop_limit),
        channel_index: channel_hash,
        next_hop: 0,
        relay_node: 0,
    };

    if let Some(frame) = RadioFrame::from_parts(&header, &enc_buf) {
        info!("[Mesh] BLE->LoRa: portnum={} to={:08x}", portnum, to);
        if ota_want_ack {
            let ack_entry = crate::domain::pending::PendingAck {
                frame: frame.clone(),
                packet_id,
                dest: to,
                deadline: Instant::now() + Duration::from_millis(WANT_ACK_TIMEOUT_MS),
                retries_left: WANT_ACK_MAX_RETRIES,
            };
            if ctx.pending_acks.push(ack_entry).is_err() {
                warn!(
                    "[Mesh] pending_acks full ({} entries), ACK tracking dropped for {:08x}",
                    ctx.pending_acks.capacity(),
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
}

async fn send_config_exchange<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, config_id: u32) {
    let my_num = ctx.device.my_node_num;
    let nodedb_count = 1u32 + ctx.node_db.len() as u32;

    // 1. MyNodeInfo
    let id = next_from_radio_id(ctx.from_radio_id);
    ctx.tx_to_ble
        .send(FromRadioMessage {
            data: encode_from_radio(
                id,
                from_radio::PayloadVariant::MyInfo(MyNodeInfo {
                    my_node_num: my_num,
                    nodedb_count,
                    min_app_version: 20300,
                    ..Default::default()
                }),
            ),
            id,
        })
        .await;

    // 2. Our own NodeInfo
    let id = next_from_radio_id(ctx.from_radio_id);
    ctx.tx_to_ble
        .send(FromRadioMessage {
            data: encode_from_radio(
                id,
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
            ),
            id,
        })
        .await;

    // 3. Metadata
    let id = next_from_radio_id(ctx.from_radio_id);
    ctx.tx_to_ble
        .send(FromRadioMessage {
            data: encode_from_radio(
                id,
                from_radio::PayloadVariant::Metadata(DeviceMetadata {
                    firmware_version: "2.5.23.0".into(),
                    device_state_version: 23,
                    has_bluetooth: true,
                    hw_model: ctx.device.hw_model as i32,
                    ..Default::default()
                }),
            ),
            id,
        })
        .await;

    // 4. All 8 channels
    for idx in 0u8..8u8 {
        let id = next_from_radio_id(ctx.from_radio_id);
        let ch_msg = if let Some(ch) = ctx.device.channels.get(idx) {
            Channel {
                index: idx as i32,
                settings: Some(ChannelSettings {
                    psk: ch.effective_psk().to_vec(),
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
        ctx.tx_to_ble
            .send(FromRadioMessage {
                data: encode_from_radio(id, from_radio::PayloadVariant::Channel(ch_msg)),
                id,
            })
            .await;
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
        let id = next_from_radio_id(ctx.from_radio_id);
        ctx.tx_to_ble
            .send(FromRadioMessage {
                data: encode_from_radio(
                    id,
                    from_radio::PayloadVariant::Config(Config {
                        payload_variant: Some(variant),
                    }),
                ),
                id,
            })
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
        let id = next_from_radio_id(ctx.from_radio_id);
        ctx.tx_to_ble
            .send(FromRadioMessage {
                data: encode_from_radio(
                    id,
                    from_radio::PayloadVariant::ModuleConfig(ModuleConfig {
                        payload_variant: Some(variant),
                    }),
                ),
                id,
            })
            .await;
    }

    // 7. NodeDB
    let mut node_nums: heapless::Vec<u32, 64> = heapless::Vec::new();
    for entry in ctx.node_db.iter() {
        node_nums.push(entry.node_num).ok();
    }
    for num in &node_nums {
        let from_radio_id = next_from_radio_id(ctx.from_radio_id);
        if let Some(entry) = ctx.node_db.get(*num) {
            let data =
                crate::domain::handlers::util::make_node_info_from_radio(from_radio_id, entry);
            ctx.tx_to_ble
                .send(FromRadioMessage {
                    data,
                    id: from_radio_id,
                })
                .await;
        }
    }

    // 8. ConfigCompleteId
    let id = next_from_radio_id(ctx.from_radio_id);
    ctx.tx_to_ble
        .send(FromRadioMessage {
            data: encode_from_radio(id, from_radio::PayloadVariant::ConfigCompleteId(config_id)),
            id,
        })
        .await;

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

        let channel = ctx
            .device
            .channels
            .find_by_hash(header.channel_index, ctx.device.modem_preset.display_name());
        let channel_index = channel.map(|c| c.index).unwrap_or(0);

        let mut payload: heapless::Vec<u8, 256> = heapless::Vec::new();
        payload.extend_from_slice(frame.payload()).ok();

        if let Some(ch) = channel
            && ch.is_encrypted()
            && !payload.is_empty()
        {
            let psk = ch.effective_psk();
            let mut psk_copy = [0u8; 32];
            let psk_len_copy = psk.len().min(32);
            psk_copy[..psk_len_copy].copy_from_slice(&psk[..psk_len_copy]);
            if crypto::crypt_packet(
                &psk_copy[..psk_len_copy],
                header.packet_id,
                header.sender,
                &mut payload,
            )
            .is_err()
            {
                continue;
            }
        }

        let data_msg = match Data::decode(payload.as_slice()) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let portnum = data_msg.portnum;
        let inner_payload = data_msg.payload;

        let from_radio_id = next_from_radio_id(ctx.from_radio_id);
        let data = make_from_radio_packet(
            from_radio_id,
            &header,
            channel_index,
            portnum,
            &inner_payload,
            RadioMetadata { snr: 0, rssi: 0 },
        );
        if ctx
            .tx_to_ble
            .try_send(FromRadioMessage {
                data,
                id: from_radio_id,
            })
            .is_err()
        {
            warn!(
                "[Mesh] BLE TX queue full, dropped stored frame id={}",
                from_radio_id
            );
        }
    }
    info!("[Mesh] Store-and-forward replay complete");
}
