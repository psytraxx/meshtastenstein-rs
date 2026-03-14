//! Mesh task: central orchestrator for Meshtastic protocol
//!
//! Uses prost-generated types for all protobuf encode/decode.
//! Raw OTA frame handling (16-byte header, RadioFrame) is unchanged.

use crate::constants::*;
use crate::inter_task::channels::{FromRadioMessage, ToRadioMessage};
use crate::mesh::crypto;
use crate::mesh::device::DeviceState;
use crate::mesh::node_db::NodeDB;
use crate::mesh::packet::{HEADER_SIZE, PacketHeader, RadioFrame};
use crate::mesh::portnum_handler;
use crate::mesh::router::MeshRouter;
use crate::proto::{
    Channel, ChannelSettings, Config, Data, FromRadio, MeshPacket, MyNodeInfo, ToRadio, config,
    from_radio, mesh_packet, to_radio,
};
use crate::tasks::led_task::{LedCommand, LedPattern};
use crate::tasks::lora_task::RadioMetadata;
use embassy_futures::select::{Either, Either4, select, select4};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Receiver, Sender};
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Ticker, Timer};
use log::{debug, info, warn};
use prost::Message;

/// Pending rebroadcast
struct PendingRebroadcast {
    frame: RadioFrame,
    deadline: Instant,
}

/// Central mesh orchestrator
pub struct MeshOrchestrator {
    // LoRa channels
    tx_to_lora: Sender<'static, CriticalSectionRawMutex, RadioFrame, 5>,
    rx_from_lora: Receiver<'static, CriticalSectionRawMutex, (RadioFrame, RadioMetadata), 5>,

    // BLE channels
    tx_to_ble: Sender<'static, CriticalSectionRawMutex, FromRadioMessage, 10>,
    rx_from_ble: Receiver<'static, CriticalSectionRawMutex, ToRadioMessage, 5>,

    // Control channels
    connection_state_rx: Receiver<'static, CriticalSectionRawMutex, bool, 1>,
    led_commands: Sender<'static, CriticalSectionRawMutex, LedCommand, 5>,
    activity_signal: &'static Signal<CriticalSectionRawMutex, Instant>,
    radio_stats: &'static Signal<CriticalSectionRawMutex, (i16, i8)>,

    // Mesh state
    device: DeviceState,
    node_db: NodeDB,
    router: MeshRouter,

    // Pending rebroadcast
    pending_rebroadcast: Option<PendingRebroadcast>,

    // Connection state
    ble_connected: bool,

    // FromRadio message counter (monotonically increasing ID for phone)
    from_radio_id: u32,
}

impl MeshOrchestrator {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx_to_lora: Sender<'static, CriticalSectionRawMutex, RadioFrame, 5>,
        rx_from_lora: Receiver<'static, CriticalSectionRawMutex, (RadioFrame, RadioMetadata), 5>,
        tx_to_ble: Sender<'static, CriticalSectionRawMutex, FromRadioMessage, 10>,
        rx_from_ble: Receiver<'static, CriticalSectionRawMutex, ToRadioMessage, 5>,
        connection_state_rx: Receiver<'static, CriticalSectionRawMutex, bool, 1>,
        led_commands: Sender<'static, CriticalSectionRawMutex, LedCommand, 5>,
        activity_signal: &'static Signal<CriticalSectionRawMutex, Instant>,
        radio_stats: &'static Signal<CriticalSectionRawMutex, (i16, i8)>,
        mac: &[u8; 6],
    ) -> Self {
        let device = DeviceState::new(mac);
        let node_num = device.my_node_num;
        info!(
            "[Mesh] Initializing orchestrator. Node: {:08x} ({})",
            node_num,
            device.long_name.as_str()
        );

        Self {
            tx_to_lora,
            rx_from_lora,
            tx_to_ble,
            rx_from_ble,
            connection_state_rx,
            led_commands,
            activity_signal,
            radio_stats,
            node_db: NodeDB::new(node_num),
            router: MeshRouter::new(node_num),
            device,
            pending_rebroadcast: None,
            ble_connected: false,
            from_radio_id: 1,
        }
    }

    fn signal_activity(&self) {
        self.activity_signal.signal(Instant::now());
    }

    fn next_from_radio_id(&mut self) -> u32 {
        let id = self.from_radio_id;
        self.from_radio_id = self.from_radio_id.wrapping_add(1).max(1);
        id
    }

    /// Run the mesh orchestrator loop
    pub async fn run(&mut self) -> ! {
        info!("[Mesh] Starting mesh orchestrator loop...");
        let mut heartbeat = Ticker::every(Duration::from_millis(LED_HEARTBEAT_INTERVAL_MS));

        loop {
            // Rebroadcast timer
            let rebroadcast_fut = async {
                match self.pending_rebroadcast {
                    Some(ref p) => Timer::at(p.deadline).await,
                    None => core::future::pending::<()>().await,
                }
            };

            match select(
                rebroadcast_fut,
                select4(
                    self.rx_from_lora.receive(),
                    self.rx_from_ble.receive(),
                    self.connection_state_rx.receive(),
                    heartbeat.next(),
                ),
            )
            .await
            {
                Either::First(_) => {
                    if let Some(pending) = self.pending_rebroadcast.take() {
                        debug!("[Mesh] Sending rebroadcast");
                        self.tx_to_lora.send(pending.frame).await;
                    }
                }
                Either::Second(Either4::First((frame, metadata))) => {
                    self.signal_activity();
                    self.radio_stats.signal((metadata.rssi, metadata.snr));
                    self.handle_lora_rx(frame, metadata).await;
                }
                Either::Second(Either4::Second(msg)) => {
                    self.signal_activity();
                    self.handle_ble_rx(msg).await;
                }
                Either::Second(Either4::Third(connected)) => {
                    self.ble_connected = connected;
                    info!(
                        "[Mesh] BLE {}",
                        if connected {
                            "connected"
                        } else {
                            "disconnected"
                        }
                    );
                    if connected {
                        self.signal_activity();
                    }
                }
                Either::Second(Either4::Fourth(_)) => {
                    let _ = self
                        .led_commands
                        .try_send(LedCommand::Blink(LedPattern::Heartbeat));
                }
            }
        }
    }

    /// Handle a received LoRa frame
    async fn handle_lora_rx(&mut self, frame: RadioFrame, metadata: RadioMetadata) {
        let header = match frame.header() {
            Some(h) => h,
            None => {
                warn!("[Mesh] Invalid frame header");
                return;
            }
        };

        debug!(
            "[Mesh] RX: from={:08x} to={:08x} id={:08x} ch={} hop={}/{}",
            header.sender,
            header.destination,
            header.packet_id,
            header.channel_index,
            header.hop_limit(),
            header.hop_start(),
        );

        // Duplicate detection
        if self.router.is_duplicate(header.sender, header.packet_id) {
            debug!("[Mesh] Duplicate packet, dropping");
            return;
        }

        let _ = self
            .led_commands
            .try_send(LedCommand::Blink(LedPattern::SingleBlink));

        // Update NodeDB
        self.node_db.touch(header.sender, 0, metadata.snr);

        // Try to decrypt
        let channel = self.device.channels.find_by_hash(header.channel_index);
        let mut payload = heapless::Vec::<u8, 256>::new();
        payload.extend_from_slice(frame.payload()).ok();

        let channel_index = channel.map(|c| c.index).unwrap_or(0);

        if let Some(ch) = channel
            && ch.is_encrypted()
            && !payload.is_empty()
        {
            let psk = ch.effective_psk();
            let mut psk_copy = [0u8; 32];
            let psk_len = psk.len().min(32);
            psk_copy[..psk_len].copy_from_slice(&psk[..psk_len]);
            if crypto::crypt_packet(
                &psk_copy[..psk_len],
                header.packet_id,
                header.sender,
                &mut payload,
            )
            .is_err()
            {
                warn!(
                    "[Mesh] Decryption failed for channel {}",
                    header.channel_index
                );
                return;
            }
        }

        // Decode Data protobuf with prost
        let data_msg = match Data::decode(payload.as_slice()) {
            Ok(d) => d,
            Err(e) => {
                warn!("[Mesh] Could not decode Data message: {:?}", e);
                return;
            }
        };
        let portnum = data_msg.portnum as u32;
        let inner_payload = data_msg.payload;

        // Dispatch by portnum
        portnum_handler::handle_portnum(
            portnum,
            &inner_payload,
            header.sender,
            &mut self.node_db,
            0,
        );

        // Send ACK if addressed to us and want_ack set
        if header.is_for_us(self.device.my_node_num) && header.want_ack() {
            self.send_routing_ack(header.sender, header.packet_id).await;
        }

        // Forward to BLE as FromRadio { packet: MeshPacket { decoded: Data } }
        if self.ble_connected {
            let from_radio_id = self.next_from_radio_id();
            let data = make_from_radio_packet(
                from_radio_id,
                &header,
                channel_index,
                portnum,
                &inner_payload,
                metadata.snr,
                metadata.rssi,
            );
            let _ = self.tx_to_ble.try_send(FromRadioMessage { data });
        }

        // Rebroadcast decision
        if let Some(new_hop) = self
            .router
            .should_rebroadcast(header.hop_limit(), header.sender)
        {
            let mut rebroadcast_frame = frame.clone();
            if let Some(mut hdr) = rebroadcast_frame.header() {
                hdr.set_hop_limit(new_hop);
                let mut hdr_buf = [0u8; HEADER_SIZE];
                hdr.encode(&mut hdr_buf);
                rebroadcast_frame.data[..HEADER_SIZE].copy_from_slice(&hdr_buf);
            }

            let delay = self.router.rebroadcast_delay_ms(metadata.snr);
            self.pending_rebroadcast = Some(PendingRebroadcast {
                frame: rebroadcast_frame,
                deadline: Instant::now() + Duration::from_millis(delay),
            });
            debug!("[Mesh] Scheduling rebroadcast in {}ms", delay);
        }
    }

    /// Handle a ToRadio message from BLE — decode with prost
    async fn handle_ble_rx(&mut self, msg: ToRadioMessage) {
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
            Some(to_radio::PayloadVariant::Packet(pkt)) => {
                self.transmit_from_ble_packet(pkt).await;
            }
            Some(to_radio::PayloadVariant::WantConfigId(id)) => {
                info!("[Mesh] Phone wants config, id={}", id);
                self.send_config_exchange(id).await;
            }
            _ => {}
        }

        let _ = self
            .led_commands
            .try_send(LedCommand::Blink(LedPattern::DoubleBlink));
    }

    /// Transmit a decoded MeshPacket received from BLE over LoRa
    async fn transmit_from_ble_packet(&mut self, pkt: MeshPacket) {
        let (portnum, inner_payload, request_id) = match &pkt.payload_variant {
            Some(mesh_packet::PayloadVariant::Decoded(data)) => {
                (data.portnum as u32, data.payload.clone(), data.request_id)
            }
            _ => {
                warn!("[Mesh] Non-decoded packet from BLE, ignoring");
                return;
            }
        };

        if portnum == 0 && inner_payload.is_empty() {
            warn!("[Mesh] Empty MeshPacket from BLE, ignoring");
            return;
        }

        let to = pkt.to;
        let packet_id = if pkt.id != 0 {
            pkt.id
        } else {
            self.device.next_packet_id()
        };
        let hop_limit = (pkt.hop_limit as u8).min(MAX_HOP_LIMIT);
        let want_ack = pkt.want_ack;
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
        let channel = self.device.channels.get(channel_idx);
        let channel_hash = channel
            .or_else(|| self.device.channels.primary())
            .map(|c| c.hash())
            .unwrap_or(0);

        let psk_for_encrypt = channel
            .or_else(|| self.device.channels.primary())
            .filter(|c| c.is_encrypted())
            .map(|c| {
                let psk = c.effective_psk();
                let mut buf = [0u8; 32];
                let len = psk.len().min(32);
                buf[..len].copy_from_slice(&psk[..len]);
                (buf, len)
            });

        if let Some((psk_buf, psk_len)) = psk_for_encrypt {
            let _ = crypto::crypt_packet(
                &psk_buf[..psk_len],
                packet_id,
                self.device.my_node_num,
                &mut enc_buf,
            );
        }

        let header = PacketHeader {
            destination: to,
            sender: self.device.my_node_num,
            packet_id,
            flags: PacketHeader::make_flags(want_ack, false, hop_limit, hop_limit),
            channel_index: channel_hash,
            next_hop: 0,
            relay_node: 0,
        };

        if let Some(frame) = RadioFrame::from_parts(&header, &enc_buf) {
            info!("[Mesh] BLE->LoRa: portnum={} to={:08x}", portnum, to);
            self.tx_to_lora.send(frame).await;
        }
    }

    /// Send complete config exchange to phone
    async fn send_config_exchange(&mut self, config_id: u32) {
        let my_num = self.device.my_node_num;

        // 1. MyNodeInfo
        let data = encode_from_radio(
            self.next_from_radio_id(),
            from_radio::PayloadVariant::MyInfo(MyNodeInfo {
                my_node_num: my_num,
                ..Default::default()
            }),
        );
        self.tx_to_ble.send(FromRadioMessage { data }).await;

        // 2. Config { lora: LoRaConfig }
        let data = encode_from_radio(
            self.next_from_radio_id(),
            from_radio::PayloadVariant::Config(Config {
                payload_variant: Some(config::PayloadVariant::Lora(config::LoRaConfig {
                    use_preset: true,
                    modem_preset: 0, // LongFast
                    region: config::lo_ra_config::RegionCode::Eu433 as i32,
                    hop_limit: DEFAULT_HOP_LIMIT as u32,
                    tx_enabled: true,
                    tx_power: LORA_TX_POWER_DBM,
                    ..Default::default()
                })),
            }),
        );
        self.tx_to_ble.send(FromRadioMessage { data }).await;

        // 3. Active channels — collect first to release borrow on self.device
        struct ChData {
            index: u8,
            psk: [u8; 32],
            psk_len: usize,
            name: [u8; 12],
            name_len: usize,
            role: i32,
        }
        let mut channel_data: heapless::Vec<ChData, 8> = heapless::Vec::new();
        for ch in self.device.channels.active_channels() {
            let psk_src = ch.effective_psk();
            let psk_len = psk_src.len().min(32);
            let mut psk = [0u8; 32];
            psk[..psk_len].copy_from_slice(&psk_src[..psk_len]);
            let name_src = ch.name.as_bytes();
            let name_len = name_src.len().min(12);
            let mut name = [0u8; 12];
            name[..name_len].copy_from_slice(&name_src[..name_len]);
            channel_data
                .push(ChData {
                    index: ch.index,
                    psk,
                    psk_len,
                    name,
                    name_len,
                    role: ch.role as i32,
                })
                .ok();
        }
        for ch in &channel_data {
            let name_str = core::str::from_utf8(&ch.name[..ch.name_len]).unwrap_or("");
            let data = encode_from_radio(
                self.next_from_radio_id(),
                from_radio::PayloadVariant::Channel(Channel {
                    index: ch.index as i32,
                    settings: Some(ChannelSettings {
                        psk: ch.psk[..ch.psk_len].to_vec(),
                        name: name_str.into(),
                        ..Default::default()
                    }),
                    role: ch.role,
                }),
            );
            self.tx_to_ble.send(FromRadioMessage { data }).await;
        }

        // 4. config_complete_id — signals end of config exchange
        let data = encode_from_radio(
            self.next_from_radio_id(),
            from_radio::PayloadVariant::ConfigCompleteId(config_id),
        );
        self.tx_to_ble.send(FromRadioMessage { data }).await;

        info!(
            "[Mesh] Config exchange complete: {} channel(s), id={}",
            channel_data.len(),
            config_id
        );
    }

    /// Send a routing ACK for a received packet
    async fn send_routing_ack(&mut self, dest: u32, request_id: u32) {
        debug!(
            "[Mesh] Sending ACK to {:08x} for packet {:08x}",
            dest, request_id
        );

        // Empty Routing payload = ACK success
        let mut enc_buf = Data {
            portnum: 5, // ROUTING_APP
            request_id,
            ..Default::default()
        }
        .encode_to_vec();

        let packet_id = self.device.next_packet_id();

        if let Some(ch) = self.device.channels.primary()
            && ch.is_encrypted()
        {
            let mut psk_copy = [0u8; 32];
            let psk = ch.effective_psk();
            let psk_len = psk.len().min(32);
            psk_copy[..psk_len].copy_from_slice(&psk[..psk_len]);
            let _ = crypto::crypt_packet(
                &psk_copy[..psk_len],
                packet_id,
                self.device.my_node_num,
                &mut enc_buf,
            );
        }

        let channel_hash = self
            .device
            .channels
            .primary()
            .map(|c| c.hash())
            .unwrap_or(0);

        let header = PacketHeader {
            destination: dest,
            sender: self.device.my_node_num,
            packet_id,
            flags: PacketHeader::make_flags(false, false, DEFAULT_HOP_LIMIT, DEFAULT_HOP_LIMIT),
            channel_index: channel_hash,
            next_hop: 0,
            relay_node: 0,
        };

        if let Some(frame) = RadioFrame::from_parts(&header, &enc_buf) {
            self.tx_to_lora.send(frame).await;
        }
    }
}

// ============================================================================
// Protobuf helpers — encode FromRadio messages using prost
// ============================================================================

/// Encode a `FromRadio` message into a heapless byte buffer ready for BLE
fn encode_from_radio(id: u32, variant: from_radio::PayloadVariant) -> heapless::Vec<u8, 512> {
    let bytes = FromRadio {
        id,
        payload_variant: Some(variant),
    }
    .encode_to_vec();
    let mut out = heapless::Vec::new();
    out.extend_from_slice(&bytes).ok();
    out
}

/// Build `FromRadio { packet: MeshPacket { decoded: Data } }` for a received LoRa packet
#[allow(clippy::too_many_arguments)]
fn make_from_radio_packet(
    from_radio_id: u32,
    header: &PacketHeader,
    channel_index: u8,
    portnum: u32,
    payload: &[u8],
    snr: i8,
    rssi: i16,
) -> heapless::Vec<u8, 512> {
    let mesh_pkt = MeshPacket {
        from: header.sender,
        to: header.destination,
        channel: channel_index as u32,
        id: header.packet_id,
        rx_snr: snr as f32,
        hop_limit: header.hop_limit() as u32,
        hop_start: header.hop_start() as u32,
        want_ack: header.want_ack(),
        rx_rssi: rssi as i32,
        payload_variant: Some(mesh_packet::PayloadVariant::Decoded(Data {
            portnum: portnum as i32,
            payload: payload.to_vec(),
            ..Default::default()
        })),
        ..Default::default()
    };
    encode_from_radio(from_radio_id, from_radio::PayloadVariant::Packet(mesh_pkt))
}
