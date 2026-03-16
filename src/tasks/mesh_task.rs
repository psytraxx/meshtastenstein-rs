//! Mesh task: central orchestrator for Meshtastic protocol
//!
//! Uses prost-generated types for all protobuf encode/decode.
//! Raw OTA frame handling (16-byte header, RadioFrame) is unchanged.

use crate::constants::*;
use crate::domain::crypto;
use crate::domain::device::{DeviceRole, DeviceState};
use crate::domain::handlers::from_app as app_handler;
use crate::domain::handlers::from_radio as radio_handler;
use crate::domain::handlers::outgoing;
use crate::domain::handlers::{AppAction, AppContext, RadioContext, admin};
use crate::domain::node_db::{NodeDB, NodeEntry};
use crate::domain::packet::{HEADER_SIZE, PacketHeader, RadioFrame};
use crate::domain::router::MeshRouter;
use crate::inter_task::channels::{
    Channels, FromRadioMessage, LedCommand, LedPattern, RadioMetadata, ToRadioMessage,
};
use crate::ports::{ConfigStorage, Storage as StorageTrait};
use crate::proto::{
    AdminMessage, Channel, ChannelSettings, Config, Data, DeviceMetadata, FromRadio, MeshPacket,
    ModuleConfig, MyNodeInfo, Neighbor, NeighborInfo, NodeInfo as ProtoNodeInfo, PortNum,
    RouteDiscovery, Routing, ToRadio, User, config, from_radio, mesh_packet, module_config,
    routing, to_radio,
};
use embassy_futures::select::{Either4, select4};
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

/// Pending outgoing packet awaiting routing ACK (M1)
struct PendingAck {
    frame: RadioFrame,
    packet_id: u32,
    dest: u32,
    deadline: Instant,
    retries_left: u8,
}

/// Central mesh orchestrator
pub struct MeshOrchestrator<S: 'static> {
    // LoRa channels
    tx_to_lora: Sender<'static, CriticalSectionRawMutex, RadioFrame, 5>,
    rx_from_lora: Receiver<'static, CriticalSectionRawMutex, (RadioFrame, RadioMetadata), 5>,

    // BLE channels
    tx_to_ble: Sender<'static, CriticalSectionRawMutex, FromRadioMessage, 20>,
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

    // Admin session passkey (sent in all get_x responses, required in set_x)
    session_passkey: [u8; 16],
    session_passkey_set: bool,

    // Flash config persistence
    storage: &'static mut S,

    // Battery level signal (from battery_task): (level_percent 0-100, voltage_mv)
    bat_level: &'static Signal<CriticalSectionRawMutex, (u8, u16)>,

    // Channel utilization signal (from lora_task): (channel_util_pct, air_util_tx_pct)
    channel_util_signal: &'static Signal<CriticalSectionRawMutex, (f32, f32)>,

    // BLE → Mesh: Bond bytes to persist in NVS
    bond_save_rx: Receiver<'static, CriticalSectionRawMutex, [u8; 48], 1>,

    // M1: Pending ACK tracking
    pending_acks: heapless::Vec<PendingAck, 8>,

    // M6: Our own position for periodic re-broadcast
    my_position_bytes: heapless::Vec<u8, 64>,
    last_position_tx: Instant,

    // Cached "!XXXXXXXX" node ID string (avoids repeated heap allocation)
    node_id_str: alloc::string::String,

    // Last time we broadcast device telemetry over LoRa
    last_lora_telemetry: Option<Instant>,

    // Boot time for uptime calculation
    boot_time: Instant,

    // Last time we sent a NodeInfo (for throttling)
    last_nodeinfo_tx: Option<Instant>,

    // Channel utilization percentage (updated by lora_task via signal)
    channel_utilization: f32,

    // Air utilization TX percentage
    air_util_tx: f32,

    // Last time we broadcast NeighborInfo
    last_neighborinfo_tx: Option<Instant>,
}

impl<S: StorageTrait + ConfigStorage> MeshOrchestrator<S> {
    pub fn new(channels: &'static Channels, mac: &[u8; 6], storage: &'static mut S) -> Self {
        let tx_to_lora = channels.lora_tx.sender();
        let rx_from_lora = channels.lora_rx.receiver();
        let tx_to_ble = channels.ble_tx.sender();
        let rx_from_ble = channels.ble_rx.receiver();
        let connection_state_rx = channels.conn_state.receiver();
        let led_commands = channels.led_cmd.sender();
        let activity_signal = &channels.activity;
        let radio_stats = &channels.radio_stats;
        let bat_level = &channels.bat_level;
        let bond_save_rx = channels.bond_save.receiver();
        let channel_util_signal = &channels.channel_util;

        let mut device = DeviceState::new(mac);
        let node_num = device.my_node_num;

        // Apply saved config if present
        storage.load_state(&mut device);

        info!(
            "[Mesh] Initializing orchestrator. Node: {:08x} ({})",
            node_num,
            device.long_name.as_str()
        );
        if let Some(ch) = device.channels.primary() {
            info!(
                "[Mesh] Primary channel: name='{}' hash=0x{:02x} encrypted={} psk_len={}",
                ch.name.as_str(),
                ch.hash(device.modem_preset.display_name()),
                ch.is_encrypted(),
                ch.effective_psk().len()
            );
        }

        let node_id_str = build_node_id_string(node_num);

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
            session_passkey: [0u8; 16],
            session_passkey_set: false,
            storage,
            bat_level,
            bond_save_rx,
            channel_util_signal,
            pending_acks: heapless::Vec::new(),
            my_position_bytes: heapless::Vec::new(),
            last_position_tx: Instant::now(),
            node_id_str,
            last_lora_telemetry: None,
            boot_time: Instant::now(),
            last_nodeinfo_tx: None,
            channel_utilization: 0.0,
            air_util_tx: 0.0,
            last_neighborinfo_tx: None,
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

        // Announce ourselves on the mesh shortly after boot
        Timer::after(Duration::from_millis(NODEINFO_BOOT_DELAY_MS)).await;
        self.broadcast_nodeinfo().await;

        let mut heartbeat = Ticker::every(Duration::from_millis(LED_HEARTBEAT_INTERVAL_MS));

        loop {
            // Persist any new bond bytes from BLE task (non-blocking poll)
            if let Ok(bytes) = self.bond_save_rx.try_receive() {
                self.storage.save_bond(&bytes);
            }

            // Update channel utilization from lora_task (non-blocking poll)
            if let Some((ch_util, air_tx)) = self.channel_util_signal.try_take() {
                self.channel_utilization = ch_util;
                self.air_util_tx = air_tx;
            }

            // Rebroadcast timer
            let rebroadcast_fut = async {
                match self.pending_rebroadcast {
                    Some(ref p) => Timer::at(p.deadline).await,
                    None => core::future::pending::<()>().await,
                }
            };

            // ACK timeout timer (M1)
            let ack_timeout_fut = async {
                let earliest = self.pending_acks.iter().fold(None::<Instant>, |acc, a| {
                    Some(match acc {
                        None => a.deadline,
                        Some(prev) => {
                            if a.deadline < prev {
                                a.deadline
                            } else {
                                prev
                            }
                        }
                    })
                });
                match earliest {
                    Some(deadline) => Timer::at(deadline).await,
                    None => core::future::pending::<()>().await,
                }
            };

            match select4(
                rebroadcast_fut,
                ack_timeout_fut,
                self.bat_level.wait(),
                select4(
                    self.rx_from_lora.receive(),
                    self.rx_from_ble.receive(),
                    self.connection_state_rx.receive(),
                    heartbeat.next(),
                ),
            )
            .await
            {
                Either4::First(_) => {
                    if let Some(pending) = self.pending_rebroadcast.take() {
                        debug!("[Mesh] Sending rebroadcast");
                        self.tx_to_lora.send(pending.frame).await;
                    }
                }
                Either4::Second(_) => {
                    self.check_ack_timeouts().await;
                }
                Either4::Third((level, voltage_mv)) => {
                    self.send_device_telemetry(level, voltage_mv).await;
                }
                Either4::Fourth(Either4::First((frame, metadata))) => {
                    self.signal_activity();
                    self.radio_stats.signal((metadata.rssi, metadata.snr));
                    self.handle_lora_rx(frame, metadata).await;
                }
                Either4::Fourth(Either4::Second(msg)) => {
                    self.signal_activity();
                    self.handle_ble_rx(msg).await;
                }
                Either4::Fourth(Either4::Third(connected)) => {
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
                Either4::Fourth(Either4::Fourth(_)) => {
                    let _ = self
                        .led_commands
                        .try_send(LedCommand::Blink(LedPattern::Heartbeat));
                    // Periodic NodeInfo re-broadcast
                    let nodeinfo_interval = self.nodeinfo_interval_ms();
                    if nodeinfo_interval > 0 {
                        let last = self.last_nodeinfo_tx.unwrap_or(Instant::MIN);
                        if last.elapsed() >= Duration::from_millis(nodeinfo_interval) {
                            self.broadcast_nodeinfo().await;
                        }
                    }
                    // Periodic NeighborInfo broadcast (every 6 hours)
                    let ni_due = self
                        .last_neighborinfo_tx
                        .map(|t| {
                            t.elapsed() >= Duration::from_millis(NEIGHBORINFO_BROADCAST_INTERVAL_MS)
                        })
                        .unwrap_or(
                            // First broadcast after 6 hours from boot
                            self.boot_time.elapsed()
                                >= Duration::from_millis(NEIGHBORINFO_BROADCAST_INTERVAL_MS),
                        );
                    if ni_due && self.channel_utilization < CHANNEL_UTIL_THRESHOLD {
                        self.broadcast_neighborinfo().await;
                    }

                    // M6: Periodic position re-broadcast (gated by channel utilization)
                    let pos_interval = self.position_interval_ms();
                    if pos_interval > 0
                        && self.channel_utilization < CHANNEL_UTIL_THRESHOLD
                        && !self.my_position_bytes.is_empty()
                        && self.last_position_tx.elapsed() >= Duration::from_millis(pos_interval)
                    {
                        self.broadcast_position().await;
                    }
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

        info!(
            "[Mesh] RX: from={:08x} to={:08x} id={:08x} ch=0x{:02x} hop={}/{} rssi={} snr={}",
            header.sender,
            header.destination,
            header.packet_id,
            header.channel_index,
            header.hop_limit(),
            header.hop_start(),
            metadata.rssi,
            metadata.snr,
        );

        // Implicit ACK: if we hear our own packet being rebroadcast, cancel pending retransmit
        if header.sender == self.device.my_node_num {
            let idx = self
                .pending_acks
                .iter()
                .position(|a| a.packet_id == header.packet_id);
            if let Some(i) = idx {
                info!(
                    "[Mesh] Implicit ACK: heard rebroadcast of {:08x}",
                    header.packet_id
                );
                self.pending_acks.swap_remove(i);
            }
            debug!("[Mesh] Own packet rebroadcast heard, dropping");
            return;
        }

        // Duplicate detection (pass current time so router stays platform-free)
        let now_ms = Instant::now().as_ticks() * 1_000 / embassy_time::TICK_HZ;
        if self
            .router
            .is_duplicate(header.sender, header.packet_id, now_ms)
        {
            debug!("[Mesh] Duplicate packet, dropping");
            return;
        }

        let _ = self
            .led_commands
            .try_send(LedCommand::Blink(LedPattern::SingleBlink));

        // Update NodeDB
        self.node_db.touch(header.sender, 0, metadata.snr);

        // Try to decrypt
        let preset_name = self.device.modem_preset.display_name();
        let channel = self
            .device
            .channels
            .find_by_hash(header.channel_index, preset_name);
        if channel.is_none() {
            warn!(
                "[Mesh] No channel matched hash=0x{:02x} — our primary hash=0x{:02x}; will try plaintext",
                header.channel_index,
                self.device
                    .channels
                    .primary()
                    .map(|c| c.hash(preset_name))
                    .unwrap_or(0)
            );
        }
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
                    "[Mesh] Decryption failed for channel hash=0x{:02x}",
                    header.channel_index
                );
                return;
            }
            info!(
                "[Mesh] Decrypted {} bytes with ch_hash=0x{:02x}",
                payload.len(),
                header.channel_index
            );
        }

        // Decode Data protobuf with prost
        let data_msg = match Data::decode(payload.as_slice()) {
            Ok(d) => d,
            Err(e) => {
                warn!("[Mesh] Could not decode Data message: {:?}", e);
                return;
            }
        };
        let portnum = data_msg.portnum;
        let want_response = data_msg.want_response;
        let request_id = data_msg.request_id;
        let inner_payload = data_msg.payload;
        info!(
            "[Mesh] Decoded portnum={} payload={}B from={:08x}",
            portnum,
            inner_payload.len(),
            header.sender
        );

        // Central portnum dispatch — pure, returns flags for side-effects below
        let ph = radio_handler::dispatch(
            &RadioContext {
                portnum,
                payload: &inner_payload,
                sender: header.sender,
                want_response,
                request_id,
                addressed_to_us: header.is_for_us(self.device.my_node_num),
            },
            &mut self.node_db,
        );

        // M1: Clear pending ACK if routing ACK received
        if let Some(ack_id) = ph.clear_ack_id {
            let idx = self.pending_acks.iter().position(|a| a.packet_id == ack_id);
            if let Some(i) = idx {
                self.pending_acks.swap_remove(i);
                info!("[Mesh] ACK received for packet {:08x}", ack_id);
            }
        }

        // I4: Buffer text messages when BLE is disconnected
        if ph.buffer_if_offline && !self.ble_connected {
            let _ = self.storage.add(&frame);
            info!("[Mesh] Buffered TEXT_MESSAGE from {:08x}", header.sender);
        }

        // Respond to NodeInfo requests (throttled to NODEINFO_MIN_INTERVAL_MS)
        if ph.reply_with_nodeinfo {
            let throttled = self
                .last_nodeinfo_tx
                .map(|t| t.elapsed() < Duration::from_millis(NODEINFO_MIN_INTERVAL_MS))
                .unwrap_or(false);
            if throttled {
                debug!(
                    "[Mesh] NodeInfo request from {:08x} throttled",
                    header.sender
                );
            } else {
                info!(
                    "[Mesh] NodeInfo request from {:08x}, sending response",
                    header.sender
                );
                self.send_nodeinfo(header.sender, false).await;
                self.last_nodeinfo_tx = Some(Instant::now());
            }
        }

        // Traceroute reply: append our node_num + SNR, return RouteDiscovery to sender
        if portnum == PortNum::TracerouteApp as i32
            && header.is_for_us(self.device.my_node_num)
            && want_response
        {
            self.handle_traceroute_request(
                header.sender,
                header.packet_id,
                &inner_payload,
                metadata.snr,
            )
            .await;
        }

        // Send ACK if addressed to us and want_ack set
        if header.is_for_us(self.device.my_node_num) && header.want_ack() {
            self.send_routing_ack(header.sender, header.packet_id).await;
        }

        // Forward to BLE as FromRadio { packet: MeshPacket { decoded: Data } }
        if self.ble_connected && ph.forward_to_ble {
            let from_radio_id = self.next_from_radio_id();
            let data = make_from_radio_packet(
                from_radio_id,
                &header,
                channel_index,
                portnum,
                &inner_payload,
                metadata,
            );
            if self
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

            // M4: also send FromRadio { node_info } after NodeInfo/Position updates
            // so the phone's node list stays current
            if ph.notify_ble_of_node_update {
                let node_from_radio_id = self.next_from_radio_id();
                if let Some(entry) = self.node_db.get(header.sender) {
                    let data = make_node_info_from_radio(node_from_radio_id, entry);
                    if self
                        .tx_to_ble
                        .try_send(FromRadioMessage {
                            data,
                            id: node_from_radio_id,
                        })
                        .is_err()
                    {
                        warn!(
                            "[Mesh] BLE TX queue full, dropped NodeInfo id={}",
                            node_from_radio_id
                        );
                    }
                }
            }
        }

        // Rebroadcast decision (gated by role)
        if self.should_rebroadcast_for_role()
            && let Some(new_hop) = self
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
                self.replay_stored_frames().await;
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

        let to = pkt.to;
        let from = pkt.from;
        let req_pkt_id = pkt.id;

        match app_handler::dispatch(&AppContext {
            portnum,
            payload: &inner_payload,
            to,
            my_node_num: self.device.my_node_num,
        }) {
            AppAction::Drop => {
                warn!("[Mesh] Empty MeshPacket from BLE, ignoring");
                return;
            }
            AppAction::SavePositionAndTransmit => {
                // M6: Save position payload for periodic re-broadcast
                self.my_position_bytes.clear();
                self.my_position_bytes
                    .extend_from_slice(&inner_payload)
                    .ok();
            }
            AppAction::HandleAdminLocally => {
                self.handle_admin_from_ble(from, req_pkt_id, &inner_payload)
                    .await;
                // Send routing ACK so the app knows the admin message was received.
                // The app waits for this before sending follow-up commands (e.g. RebootSeconds).
                if pkt.want_ack {
                    self.send_ble_routing_ack(from, req_pkt_id).await;
                }
                return;
            }
            AppAction::Transmit => {}
        }

        let packet_id = if pkt.id != 0 {
            pkt.id
        } else {
            self.device.next_packet_id()
        };
        let hop_limit = (pkt.hop_limit as u8).min(MAX_HOP_LIMIT);
        // Text messages auto-set want_ack (matches official firmware behavior)
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
        let preset_name = self.device.modem_preset.display_name();
        let channel = self.device.channels.get(channel_idx);
        let channel_hash = channel
            .or_else(|| self.device.channels.primary())
            .map(|c| c.hash(preset_name))
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

        // Broadcast packets don't get mesh-level ACKs; only unicast can be ACK'd on the wire
        let is_broadcast = to == 0xFFFF_FFFF;
        let ota_want_ack = want_ack && !is_broadcast;

        let header = PacketHeader {
            destination: to,
            sender: self.device.my_node_num,
            packet_id,
            flags: PacketHeader::make_flags(ota_want_ack, false, hop_limit, hop_limit),
            channel_index: channel_hash,
            next_hop: 0,
            relay_node: 0,
        };

        if let Some(frame) = RadioFrame::from_parts(&header, &enc_buf) {
            info!("[Mesh] BLE->LoRa: portnum={} to={:08x}", portnum, to);
            if ota_want_ack {
                let ack_entry = PendingAck {
                    frame: frame.clone(),
                    packet_id,
                    dest: to,
                    deadline: Instant::now() + Duration::from_millis(WANT_ACK_TIMEOUT_MS),
                    retries_left: WANT_ACK_MAX_RETRIES,
                };
                if self.pending_acks.push(ack_entry).is_err() {
                    warn!(
                        "[Mesh] pending_acks full ({} entries), ACK tracking dropped for {:08x}",
                        self.pending_acks.capacity(),
                        packet_id
                    );
                } else {
                    info!("[Mesh] Tracking ACK for packet {:08x}", packet_id);
                }
            }
            self.tx_to_lora.send(frame).await;

            // Send local "sent" confirmation to the phone so the app knows the packet
            // was queued for transmission (Routing { error_reason: NONE }).
            let ack_dest = if from == 0 {
                self.device.my_node_num
            } else {
                from
            };
            self.send_ble_routing_ack(ack_dest, req_pkt_id).await;
        }
    }

    /// Send complete config exchange to phone
    async fn send_config_exchange(&mut self, config_id: u32) {
        let my_num = self.device.my_node_num;
        // nodedb_count = our own node (1) + known remote nodes
        let nodedb_count = 1u32 + self.node_db.len() as u32;

        // 1. MyNodeInfo
        let id = self.next_from_radio_id();
        self.tx_to_ble
            .send(make_from_radio_msg(
                id,
                from_radio::PayloadVariant::MyInfo(MyNodeInfo {
                    my_node_num: my_num,
                    nodedb_count,
                    min_app_version: 20300, // minimum app version (2.3.0)
                    ..Default::default()
                }),
            ))
            .await;

        // 2. Our own NodeInfo (phone needs this to show us in the node list)
        let id = self.next_from_radio_id();
        self.tx_to_ble
            .send(make_from_radio_msg(
                id,
                from_radio::PayloadVariant::NodeInfo(ProtoNodeInfo {
                    num: my_num,
                    user: Some(User {
                        id: self.node_id_str.clone(),
                        long_name: self.device.long_name.as_str().into(),
                        short_name: self.device.short_name.as_str().into(),
                        hw_model: self.device.hw_model as i32,
                        is_licensed: false,
                        ..Default::default()
                    }),
                    is_favorite: true,
                    ..Default::default()
                }),
            ))
            .await;

        // 3. Metadata
        let id = self.next_from_radio_id();
        self.tx_to_ble
            .send(make_from_radio_msg(
                id,
                from_radio::PayloadVariant::Metadata(DeviceMetadata {
                    firmware_version: "2.5.23.0".into(),
                    device_state_version: 23,
                    has_bluetooth: true,
                    hw_model: self.device.hw_model as i32,
                    ..Default::default()
                }),
            ))
            .await;

        // 4. All 8 channels (indices 0-7, disabled if not configured)
        // Collect active channels first to release borrow on self.device
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
        let num_channels = channel_data.len();
        for idx in 0u8..8u8 {
            let found = channel_data.iter().find(|c| c.index == idx);
            let id = self.next_from_radio_id();
            let ch_msg = if let Some(ch) = found {
                let name_str = core::str::from_utf8(&ch.name[..ch.name_len]).unwrap_or("");
                Channel {
                    index: idx as i32,
                    settings: Some(ChannelSettings {
                        psk: ch.psk[..ch.psk_len].to_vec(),
                        name: name_str.into(),
                        ..Default::default()
                    }),
                    role: ch.role,
                }
            } else {
                Channel {
                    index: idx as i32,
                    settings: None,
                    role: 0, // DISABLED
                }
            };
            self.tx_to_ble
                .send(make_from_radio_msg(
                    id,
                    from_radio::PayloadVariant::Channel(ch_msg),
                ))
                .await;
        }

        // 5. All Config types (phone state machine requires all types before completing)
        let lora_cfg = admin::build_lora_config(&self.device);
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
            let id = self.next_from_radio_id();
            self.tx_to_ble
                .send(make_from_radio_msg(
                    id,
                    from_radio::PayloadVariant::Config(Config {
                        payload_variant: Some(variant),
                    }),
                ))
                .await;
        }

        // 6. All ModuleConfig types (phone state machine requires all types)
        for variant in [
            module_config::PayloadVariant::Mqtt(module_config::MqttConfig::default()),
            module_config::PayloadVariant::Serial(module_config::SerialConfig::default()),
            module_config::PayloadVariant::ExternalNotification(
                module_config::ExternalNotificationConfig::default(),
            ),
            module_config::PayloadVariant::StoreForward(
                module_config::StoreForwardConfig::default(),
            ),
            module_config::PayloadVariant::RangeTest(module_config::RangeTestConfig::default()),
            module_config::PayloadVariant::Telemetry(module_config::TelemetryConfig::default()),
            module_config::PayloadVariant::CannedMessage(
                module_config::CannedMessageConfig::default(),
            ),
            module_config::PayloadVariant::Audio(module_config::AudioConfig::default()),
            module_config::PayloadVariant::RemoteHardware(
                module_config::RemoteHardwareConfig::default(),
            ),
            module_config::PayloadVariant::NeighborInfo(
                module_config::NeighborInfoConfig::default(),
            ),
            module_config::PayloadVariant::AmbientLighting(
                module_config::AmbientLightingConfig::default(),
            ),
            module_config::PayloadVariant::DetectionSensor(
                module_config::DetectionSensorConfig::default(),
            ),
            module_config::PayloadVariant::Paxcounter(module_config::PaxcounterConfig::default()),
        ] {
            let id = self.next_from_radio_id();
            self.tx_to_ble
                .send(make_from_radio_msg(
                    id,
                    from_radio::PayloadVariant::ModuleConfig(ModuleConfig {
                        payload_variant: Some(variant),
                    }),
                ))
                .await;
        }

        // 7. NodeDB — send all known nodes so phone populates its node list
        let mut node_nums: heapless::Vec<u32, 64> = heapless::Vec::new();
        for entry in self.node_db.iter() {
            node_nums.push(entry.node_num).ok();
        }
        let node_count = node_nums.len();
        for num in &node_nums {
            let from_radio_id = self.next_from_radio_id();
            if let Some(entry) = self.node_db.get(*num) {
                let data = make_node_info_from_radio(from_radio_id, entry);
                self.tx_to_ble
                    .send(FromRadioMessage {
                        data,
                        id: from_radio_id,
                    })
                    .await;
            }
        }

        // 8. ConfigCompleteId — signals end of config exchange
        let id = self.next_from_radio_id();
        self.tx_to_ble
            .send(make_from_radio_msg(
                id,
                from_radio::PayloadVariant::ConfigCompleteId(config_id),
            ))
            .await;

        info!(
            "[Mesh] Config exchange complete: {} channel(s), {} node(s), id={}",
            num_channels, node_count, config_id
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
            portnum: PortNum::RoutingApp as i32,
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
            .map(|c| c.hash(self.device.modem_preset.display_name()))
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

    /// Send a ROUTING_APP ACK to the phone via BLE (for admin messages with want_ack=true).
    /// Encodes Routing { error_reason: NONE } as the payload — required by the web/app clients;
    /// an empty payload causes "Unhandled case undefined" in the JS client's oneof switch.
    async fn send_ble_routing_ack(&mut self, dest: u32, request_id: u32) {
        let routing_bytes = Routing {
            variant: Some(routing::Variant::ErrorReason(0)), // 0 = NONE = success
        }
        .encode_to_vec();
        let packet_id = self.device.next_packet_id();
        let from_radio_id = self.next_from_radio_id();
        let msg = make_from_radio_msg(
            from_radio_id,
            from_radio::PayloadVariant::Packet(MeshPacket {
                from: self.device.my_node_num,
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
        );
        if self.tx_to_ble.try_send(msg).is_err() {
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

    /// Send a TELEMETRY_APP (portnum 67) packet with our device metrics.
    /// Broadcasts over LoRa (rate-limited to TELEMETRY_LORA_INTERVAL_MS).
    /// Also forwards to BLE if connected.
    /// Get the NodeInfo broadcast interval (ms) based on device role. Returns 0 for roles that never broadcast.
    fn nodeinfo_interval_ms(&self) -> u64 {
        match self.device.role {
            DeviceRole::Repeater | DeviceRole::ClientHidden => 0,
            DeviceRole::Router | DeviceRole::RouterClient => ROUTER_BROADCAST_INTERVAL_MS,
            _ => NODEINFO_BROADCAST_INTERVAL_MS,
        }
    }

    /// Get the Position broadcast interval (ms) based on device role. Returns 0 for roles that never broadcast.
    fn position_interval_ms(&self) -> u64 {
        match self.device.role {
            DeviceRole::Repeater | DeviceRole::ClientHidden => 0,
            DeviceRole::Router | DeviceRole::RouterClient => ROUTER_BROADCAST_INTERVAL_MS,
            _ => POSITION_BROADCAST_INTERVAL_MS,
        }
    }

    /// Get the Telemetry broadcast interval (ms) based on device role. Returns 0 for roles that never broadcast.
    fn telemetry_interval_ms(&self) -> u64 {
        match self.device.role {
            DeviceRole::Repeater | DeviceRole::ClientHidden => 0,
            DeviceRole::Router | DeviceRole::RouterClient => ROUTER_BROADCAST_INTERVAL_MS,
            _ => TELEMETRY_LORA_INTERVAL_MS,
        }
    }

    /// Whether this role should rebroadcast other nodes' packets
    fn should_rebroadcast_for_role(&self) -> bool {
        !matches!(
            self.device.role,
            DeviceRole::ClientMute | DeviceRole::ClientHidden
        )
    }

    /// Uptime in seconds since boot
    fn uptime_seconds(&self) -> u32 {
        self.boot_time.elapsed().as_secs() as u32
    }

    async fn send_device_telemetry(&mut self, battery_level: u8, voltage_mv: u16) {
        let voltage_v = voltage_mv as f32 / 1000.0;
        let payload = outgoing::telemetry::build_payload(
            battery_level,
            voltage_v,
            self.channel_utilization,
            self.air_util_tx,
            self.uptime_seconds(),
        );

        // --- LoRa broadcast (rate-limited) ---
        let telemetry_interval = self.telemetry_interval_ms();
        let lora_due = telemetry_interval > 0
            && self.channel_utilization < CHANNEL_UTIL_THRESHOLD
            && self
                .last_lora_telemetry
                .map(|t| t.elapsed() >= Duration::from_millis(telemetry_interval))
                .unwrap_or(true);

        if lora_due
            && self
                .lora_send(
                    PortNum::TelemetryApp as i32,
                    payload.clone(),
                    0xFFFF_FFFF,
                    false,
                )
                .await
        {
            info!(
                "[Mesh] Telemetry LoRa broadcast: battery={}% voltage={:.2}V",
                battery_level, voltage_v
            );
            self.last_lora_telemetry = Some(Instant::now());
        }

        // --- BLE forward (if connected) ---
        if self.ble_connected {
            let packet_id = self.device.next_packet_id();
            let from_radio_id = self.next_from_radio_id();
            let data = encode_from_radio(
                from_radio_id,
                from_radio::PayloadVariant::Packet(MeshPacket {
                    from: self.device.my_node_num,
                    to: 0xFFFF_FFFF,
                    id: packet_id,
                    payload_variant: Some(mesh_packet::PayloadVariant::Decoded(Data {
                        portnum: PortNum::TelemetryApp as i32,
                        payload,
                        ..Default::default()
                    })),
                    ..Default::default()
                }),
            );
            if self
                .tx_to_ble
                .try_send(FromRadioMessage {
                    data,
                    id: from_radio_id,
                })
                .is_err()
            {
                warn!(
                    "[Mesh] BLE TX queue full, dropped telemetry id={}",
                    from_radio_id
                );
            }
            debug!(
                "[Mesh] Telemetry BLE: battery={}% voltage={:.2}V",
                battery_level, voltage_v
            );
        }
    }

    /// Snapshot current device state and write it to flash.
    fn persist_config(&mut self) {
        self.storage.save_state(&self.device);
    }

    /// Encrypt (if primary channel is encrypted), build a PacketHeader, and
    /// transmit to LoRa. Returns `true` if a frame was queued.
    ///
    /// `payload` is the portnum-specific proto bytes (e.g. `User`, `Telemetry`).
    /// They are wrapped in `Data { portnum, payload, want_response }` here.
    async fn lora_send(
        &mut self,
        portnum: i32,
        payload: alloc::vec::Vec<u8>,
        dest: u32,
        want_response: bool,
    ) -> bool {
        let packet_id = self.device.next_packet_id();
        let mut data_bytes = Data {
            portnum,
            payload,
            want_response,
            ..Default::default()
        }
        .encode_to_vec();

        let preset_name = self.device.modem_preset.display_name();
        let channel = self.device.channels.primary();
        let channel_hash = channel.map(|c| c.hash(preset_name)).unwrap_or(0);

        if let Some(ch) = channel
            && ch.is_encrypted()
        {
            let psk = ch.effective_psk();
            let mut psk_copy = [0u8; 32];
            let psk_len = psk.len().min(32);
            psk_copy[..psk_len].copy_from_slice(&psk[..psk_len]);
            let _ = crypto::crypt_packet(
                &psk_copy[..psk_len],
                packet_id,
                self.device.my_node_num,
                &mut data_bytes,
            );
        }

        let header = PacketHeader {
            destination: dest,
            sender: self.device.my_node_num,
            packet_id,
            flags: PacketHeader::make_flags(false, false, DEFAULT_HOP_LIMIT, DEFAULT_HOP_LIMIT),
            channel_index: channel_hash,
            next_hop: 0,
            relay_node: 0,
        };

        if let Some(frame) = RadioFrame::from_parts(&header, &data_bytes) {
            self.tx_to_lora.send(frame).await;
            true
        } else {
            false
        }
    }

    /// Broadcast our NodeInfo to the mesh (destination = 0xFFFF_FFFF).
    async fn broadcast_nodeinfo(&mut self) {
        self.send_nodeinfo(0xFFFF_FFFF, false).await;
        self.last_nodeinfo_tx = Some(Instant::now());
        info!(
            "[Mesh] NodeInfo broadcast: {} ({})",
            self.device.long_name.as_str(),
            self.device.short_name.as_str()
        );
    }

    /// Send our NodeInfo to `dest`. Set `want_response=true` to solicit a reply.
    async fn send_nodeinfo(&mut self, dest: u32, want_response: bool) {
        let payload = outgoing::node_info::build_payload(&self.device, &self.node_id_str);
        if self
            .lora_send(PortNum::NodeinfoApp as i32, payload, dest, want_response)
            .await
        {
            info!("[Mesh] NodeInfo TX: to={:08x}", dest);
        }
    }

    /// Derive a session passkey from node_num on first use.
    /// Not cryptographically random, but satisfies the protocol's replay-prevention intent
    /// for local BLE sessions. Replace with RNG in a future stage.
    fn ensure_session_passkey(&mut self) {
        if self.session_passkey_set {
            return;
        }
        let n = self.device.my_node_num;
        let a = n.wrapping_mul(0x9E37_79B9);
        let b = n.wrapping_mul(0x6C62_272E);
        let c = n.wrapping_mul(0xC2B2_AE35);
        let d = n.wrapping_mul(0x27D4_EB2F);
        self.session_passkey[0..4].copy_from_slice(&a.to_le_bytes());
        self.session_passkey[4..8].copy_from_slice(&b.to_le_bytes());
        self.session_passkey[8..12].copy_from_slice(&c.to_le_bytes());
        self.session_passkey[12..16].copy_from_slice(&d.to_le_bytes());
        self.session_passkey_set = true;
        debug!("[Admin] Session passkey generated");
    }

    /// Handle an admin message that arrived via BLE addressed to us.
    /// Decodes the AdminMessage, processes the request, and sends any
    /// response back over BLE as a FromRadio packet.
    #[allow(deprecated)] // User::macaddr is deprecated in proto but still sent on-wire
    async fn handle_admin_from_ble(&mut self, requester: u32, req_pkt_id: u32, admin_bytes: &[u8]) {
        let admin_msg = match AdminMessage::decode(admin_bytes) {
            Ok(a) => a,
            Err(e) => {
                warn!("[Admin] Decode failed: {:?}", e);
                return;
            }
        };

        self.ensure_session_passkey();

        let mut ctx = admin::AdminContext {
            device: &mut self.device,
            node_id_str: &self.node_id_str,
        };
        let result = admin::dispatch(&mut ctx, admin_msg.payload_variant);

        if result.needs_persist {
            self.persist_config();
        }

        if result.factory_reset {
            self.storage.erase_config();
            self.storage.clear_bond();
        }

        if result.nodedb_reset {
            self.node_db = NodeDB::new(self.device.my_node_num);
        }

        if let Some(num) = result.remove_nodenum {
            self.node_db.remove(num);
        }

        if let Some(secs) = result.reboot_secs {
            Timer::after(Duration::from_secs(secs)).await;
            esp_hal::system::software_reset()
        }

        if let Some(variant) = result.response {
            let response_bytes = AdminMessage {
                session_passkey: self.session_passkey.to_vec(),
                payload_variant: Some(variant),
            }
            .encode_to_vec();

            let packet_id = self.device.next_packet_id();
            let from_radio_id = self.next_from_radio_id();
            self.tx_to_ble
                .send(make_from_radio_msg(
                    from_radio_id,
                    from_radio::PayloadVariant::Packet(MeshPacket {
                        from: self.device.my_node_num,
                        to: requester,
                        id: packet_id,
                        payload_variant: Some(mesh_packet::PayloadVariant::Decoded(Data {
                            portnum: PortNum::AdminApp as i32,
                            payload: response_bytes,
                            request_id: req_pkt_id,
                            ..Default::default()
                        })),
                        ..Default::default()
                    }),
                ))
                .await;
            debug!("[Admin] Response sent to {:08x}", requester);
        }
    }

    /// Retransmit timed-out want_ack packets or give up after max retries (M1)
    async fn check_ack_timeouts(&mut self) {
        let now = Instant::now();
        let mut i = 0;
        while i < self.pending_acks.len() {
            if now >= self.pending_acks[i].deadline {
                if self.pending_acks[i].retries_left > 0 {
                    let retries_left = self.pending_acks[i].retries_left - 1;
                    let frame = self.pending_acks[i].frame.clone();
                    let packet_id = self.pending_acks[i].packet_id;
                    let dest = self.pending_acks[i].dest;
                    info!(
                        "[Mesh] Retransmitting {:08x} to {:08x} ({} retries left)",
                        packet_id, dest, retries_left
                    );
                    self.tx_to_lora.send(frame).await;
                    self.pending_acks[i].deadline =
                        Instant::now() + Duration::from_millis(WANT_ACK_TIMEOUT_MS);
                    self.pending_acks[i].retries_left = retries_left;
                    i += 1;
                } else {
                    let packet_id = self.pending_acks[i].packet_id;
                    let dest = self.pending_acks[i].dest;
                    warn!(
                        "[Mesh] ACK timeout for {:08x} to {:08x}, giving up",
                        packet_id, dest
                    );
                    self.pending_acks.swap_remove(i);
                    // don't increment i — swap_remove puts the last element here
                }
            } else {
                i += 1;
            }
        }
    }

    /// Decrypt and forward buffered LoRa frames to BLE (I4 store-and-forward)
    async fn replay_stored_frames(&mut self) {
        let count = self.storage.count();
        if count == 0 {
            return;
        }
        info!("[Mesh] Replaying {} buffered frame(s) to BLE", count);
        while let Ok(Some(frame)) = self.storage.peek() {
            let _ = self.storage.pop();

            let header = match frame.header() {
                Some(h) => h,
                None => continue,
            };

            let channel = self.device.channels.find_by_hash(
                header.channel_index,
                self.device.modem_preset.display_name(),
            );
            let channel_index = channel.map(|c| c.index).unwrap_or(0);

            let mut payload: heapless::Vec<u8, 256> = heapless::Vec::new();
            payload.extend_from_slice(frame.payload()).ok();

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
                    continue;
                }
            }

            let data_msg = match Data::decode(payload.as_slice()) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let portnum = data_msg.portnum;
            let inner_payload = data_msg.payload;

            let from_radio_id = self.next_from_radio_id();
            let data = make_from_radio_packet(
                from_radio_id,
                &header,
                channel_index,
                portnum,
                &inner_payload,
                RadioMetadata { snr: 0, rssi: 0 },
            );
            if self
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

    /// Handle a traceroute request: decode RouteDiscovery, append our node_num + SNR, return to sender
    async fn handle_traceroute_request(
        &mut self,
        requester: u32,
        request_id: u32,
        payload: &[u8],
        snr: i8,
    ) {
        // Decode existing RouteDiscovery (may be empty for initial request)
        let mut route_disc = RouteDiscovery::decode(payload).unwrap_or_default();
        // Append our node_num and SNR (SNR scaled by 4 per protocol)
        route_disc.route.push(self.device.my_node_num);
        route_disc.snr_towards.push(snr as i32 * 4);

        let route_bytes = route_disc.encode_to_vec();
        let packet_id = self.device.next_packet_id();

        let mut data_bytes = Data {
            portnum: PortNum::TracerouteApp as i32,
            payload: route_bytes,
            request_id,
            ..Default::default()
        }
        .encode_to_vec();

        let channel_hash = self
            .device
            .channels
            .primary()
            .map(|c| c.hash(self.device.modem_preset.display_name()))
            .unwrap_or(0);

        if let Some(ch) = self.device.channels.primary()
            && ch.is_encrypted()
        {
            let psk = ch.effective_psk();
            let mut psk_copy = [0u8; 32];
            let psk_len = psk.len().min(32);
            psk_copy[..psk_len].copy_from_slice(&psk[..psk_len]);
            let _ = crypto::crypt_packet(
                &psk_copy[..psk_len],
                packet_id,
                self.device.my_node_num,
                &mut data_bytes,
            );
        }

        let header = PacketHeader {
            destination: requester,
            sender: self.device.my_node_num,
            packet_id,
            flags: PacketHeader::make_flags(false, false, DEFAULT_HOP_LIMIT, DEFAULT_HOP_LIMIT),
            channel_index: channel_hash,
            next_hop: 0,
            relay_node: 0,
        };

        if let Some(frame) = RadioFrame::from_parts(&header, &data_bytes) {
            info!(
                "[Mesh] Traceroute reply to {:08x} with {} hops",
                requester,
                route_disc.route.len()
            );
            self.tx_to_lora.send(frame).await;
        }
    }

    /// Broadcast our last known position to the mesh (M6)
    async fn broadcast_position(&mut self) {
        if self.my_position_bytes.is_empty() {
            return;
        }
        let payload = self.my_position_bytes.as_slice().to_vec();
        if self
            .lora_send(PortNum::PositionApp as i32, payload, 0xFFFF_FFFF, false)
            .await
        {
            info!("[Mesh] Broadcasting position to mesh");
            self.last_position_tx = Instant::now();
        }
    }

    /// Broadcast NeighborInfo to the mesh (list of recently heard nodes + SNR)
    async fn broadcast_neighborinfo(&mut self) {
        let mut neighbors = alloc::vec::Vec::new();
        for entry in self.node_db.iter() {
            if entry.node_num == self.device.my_node_num {
                continue;
            }
            neighbors.push(Neighbor {
                node_id: entry.node_num,
                snr: entry.snr as f32,
                last_rx_time: entry.last_heard,
                node_broadcast_interval_secs: (NODEINFO_BROADCAST_INTERVAL_MS / 1000) as u32,
            });
        }

        if neighbors.is_empty() {
            self.last_neighborinfo_tx = Some(Instant::now());
            return;
        }

        let neighbor_count = neighbors.len();
        let ni = NeighborInfo {
            node_id: self.device.my_node_num,
            last_sent_by_id: self.device.my_node_num,
            node_broadcast_interval_secs: (NEIGHBORINFO_BROADCAST_INTERVAL_MS / 1000) as u32,
            neighbors,
        };
        let ni_bytes = ni.encode_to_vec();

        if self
            .lora_send(
                PortNum::NeighborinfoApp as i32,
                ni_bytes,
                0xFFFF_FFFF,
                false,
            )
            .await
        {
            info!(
                "[Mesh] NeighborInfo broadcast: {} neighbor(s)",
                neighbor_count
            );
        }
        self.last_neighborinfo_tx = Some(Instant::now());
    }
}

// ============================================================================
// Protobuf helpers — encode FromRadio messages using prost
// ============================================================================

/// Build the Meshtastic node ID string "!XXXXXXXX" from a node number.
fn build_node_id_string(node_num: u32) -> alloc::string::String {
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

/// Build a `FromRadioMessage` with both the encoded bytes and the packet ID (for N4 FromNum)
fn make_from_radio_msg(id: u32, variant: from_radio::PayloadVariant) -> FromRadioMessage {
    FromRadioMessage {
        data: encode_from_radio(id, variant),
        id,
    }
}

/// Build `FromRadio { node_info: NodeInfo { ... } }` from a NodeDB entry
fn make_node_info_from_radio(from_radio_id: u32, entry: &NodeEntry) -> heapless::Vec<u8, 512> {
    let id = build_node_id_string(entry.node_num);

    let user = entry.user.as_ref().map(|u| {
        let mut u = u.clone();
        u.id = id;
        u
    });

    let node_info = ProtoNodeInfo {
        num: entry.node_num,
        user,
        position: entry.position, // proto::Position is Copy
        snr: entry.snr as f32,
        last_heard: entry.last_heard,
        ..Default::default()
    };
    encode_from_radio(
        from_radio_id,
        from_radio::PayloadVariant::NodeInfo(node_info),
    )
}

/// Build `FromRadio { packet: MeshPacket { decoded: Data } }` for a received LoRa packet
fn make_from_radio_packet(
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
