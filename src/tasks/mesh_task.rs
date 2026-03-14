//! Mesh task: central orchestrator for Meshtastic protocol
//!
//! Processes LoRa RX -> decrypt -> decode -> encode as FromRadio -> forward to BLE
//! Processes BLE RX (ToRadio) -> decode MeshPacket -> encrypt -> queue for LoRa TX
//! Handles want_config_id handshake so phone app proceeds past initial config exchange

use crate::constants::*;
use crate::inter_task::channels::{FromRadioMessage, ToRadioMessage};
use crate::mesh::crypto;
use crate::mesh::device::DeviceState;
use crate::mesh::node_db::NodeDB;
use crate::mesh::packet::{HEADER_SIZE, PacketHeader, RadioFrame};
use crate::mesh::portnum_handler;
use crate::mesh::router::MeshRouter;
use crate::tasks::led_task::{LedCommand, LedPattern};
use crate::tasks::lora_task::RadioMetadata;
use embassy_futures::select::{Either, Either4, select, select4};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Receiver, Sender};
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Ticker, Timer};
use log::{debug, info, warn};

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

        // Decode the Data protobuf
        let (portnum, inner_payload) = match decode_data_message(&payload) {
            Some(v) => v,
            None => {
                warn!("[Mesh] Could not decode Data message");
                return;
            }
        };

        // Dispatch by portnum (for ACK logic etc)
        portnum_handler::handle_portnum(
            portnum,
            &inner_payload,
            header.sender,
            &mut self.node_db,
            0, // TODO: real time
        );

        // Send ACK if addressed to us and want_ack set
        if header.is_for_us(self.device.my_node_num) && header.want_ack() {
            self.send_routing_ack(header.sender, header.packet_id).await;
        }

        // Forward to BLE as a proper FromRadio protobuf (ONCE, not twice)
        if self.ble_connected {
            let from_radio_id = self.next_from_radio_id();
            let mut tmp = [0u8; 512];
            let len = encode_from_radio_packet(
                &mut tmp,
                from_radio_id,
                &header,
                channel_index,
                portnum,
                &inner_payload,
                metadata.snr,
                metadata.rssi,
            );
            let mut data = heapless::Vec::<u8, 512>::new();
            data.extend_from_slice(&tmp[..len]).ok();
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

    /// Handle a ToRadio message from BLE
    async fn handle_ble_rx(&mut self, msg: ToRadioMessage) {
        let data = &msg.data;
        if data.is_empty() {
            return;
        }

        // Decode ToRadio oneof fields
        let mut i = 0;
        while i < data.len() {
            let tag = data[i];
            i += 1;
            let field = tag >> 3;
            let wire = tag & 0x07;

            match (field, wire) {
                (1, 2) => {
                    // packet: MeshPacket (length-delimited)
                    let (len, n) = decode_varint(&data[i..]);
                    i += n;
                    let end = (i + len as usize).min(data.len());
                    let pkt_bytes = &data[i..end];
                    i = end;
                    self.transmit_from_ble_packet(pkt_bytes).await;
                }
                (3, 0) => {
                    // want_config_id: uint32
                    let (config_id, n) = decode_varint(&data[i..]);
                    i += n;
                    info!("[Mesh] Phone wants config, id={}", config_id);
                    self.send_config_exchange(config_id as u32).await;
                }
                (_, 0) => {
                    let (_, n) = decode_varint(&data[i..]);
                    i += n;
                }
                (_, 2) => {
                    let (len, n) = decode_varint(&data[i..]);
                    i += n;
                    i += len as usize;
                }
                (_, 5) => {
                    i += 4;
                }
                (_, 1) => {
                    i += 8;
                }
                _ => break,
            }
        }

        let _ = self
            .led_commands
            .try_send(LedCommand::Blink(LedPattern::DoubleBlink));
    }

    /// Decode a raw MeshPacket from BLE ToRadio and transmit over LoRa
    async fn transmit_from_ble_packet(&mut self, pkt_bytes: &[u8]) {
        let mut to: u32 = 0xFFFF_FFFF;
        let mut portnum: u32 = 0;
        let mut inner_payload: heapless::Vec<u8, 256> = heapless::Vec::new();
        let mut pkt_id: u32 = 0;
        let mut hop_limit: u8 = DEFAULT_HOP_LIMIT;
        let mut want_ack = false;
        let mut channel_idx: u8 = 0;

        let mut i = 0;
        while i < pkt_bytes.len() {
            let tag = pkt_bytes[i];
            i += 1;
            let field = tag >> 3;
            let wire = tag & 0x07;

            match (field, wire) {
                (2, 5) => {
                    // to: fixed32
                    if i + 4 <= pkt_bytes.len() {
                        to = u32::from_le_bytes([
                            pkt_bytes[i],
                            pkt_bytes[i + 1],
                            pkt_bytes[i + 2],
                            pkt_bytes[i + 3],
                        ]);
                        i += 4;
                    }
                }
                (3, 0) => {
                    // channel: uint32 (varint)
                    let (v, n) = decode_varint(&pkt_bytes[i..]);
                    i += n;
                    channel_idx = v as u8;
                }
                (4, 2) => {
                    // decoded: Data (length-delimited)
                    let (len, n) = decode_varint(&pkt_bytes[i..]);
                    i += n;
                    let end = (i + len as usize).min(pkt_bytes.len());
                    let data_slice = &pkt_bytes[i..end];
                    if let Some((pnum, payload)) = decode_data_message(data_slice) {
                        portnum = pnum;
                        inner_payload = payload;
                    }
                    i = end;
                }
                (6, 5) => {
                    // id: fixed32
                    if i + 4 <= pkt_bytes.len() {
                        pkt_id = u32::from_le_bytes([
                            pkt_bytes[i],
                            pkt_bytes[i + 1],
                            pkt_bytes[i + 2],
                            pkt_bytes[i + 3],
                        ]);
                        i += 4;
                    }
                }
                (9, 0) => {
                    // hop_limit: uint32 (varint)
                    let (v, n) = decode_varint(&pkt_bytes[i..]);
                    i += n;
                    hop_limit = (v as u8).min(MAX_HOP_LIMIT);
                }
                (10, 0) => {
                    // want_ack: bool (varint)
                    let (v, n) = decode_varint(&pkt_bytes[i..]);
                    i += n;
                    want_ack = v != 0;
                }
                (_, 0) => {
                    let (_, n) = decode_varint(&pkt_bytes[i..]);
                    i += n;
                }
                (_, 2) => {
                    let (len, n) = decode_varint(&pkt_bytes[i..]);
                    i += n;
                    i += len as usize;
                }
                (_, 5) => {
                    i += 4;
                }
                (_, 1) => {
                    i += 8;
                }
                _ => break,
            }
        }

        if portnum == 0 && inner_payload.is_empty() {
            warn!("[Mesh] Empty MeshPacket from BLE, ignoring");
            return;
        }

        let packet_id = if pkt_id != 0 {
            pkt_id
        } else {
            self.device.next_packet_id()
        };

        // Encode Data payload
        let mut raw_data = [0u8; 256];
        let raw_data_len = encode_data_message(&mut raw_data, portnum, &inner_payload, 0);

        // Get channel PSK and encrypt
        let channel = self.device.channels.get(channel_idx);
        let channel_hash = channel.map(|c| c.hash()).unwrap_or_else(|| {
            self.device
                .channels
                .primary()
                .map(|c| c.hash())
                .unwrap_or(0)
        });

        let mut enc_buf = [0u8; 256];
        enc_buf[..raw_data_len].copy_from_slice(&raw_data[..raw_data_len]);

        let psk_for_encrypt: Option<([u8; 32], usize)> = channel
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
                &mut enc_buf[..raw_data_len],
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

        if let Some(frame) = RadioFrame::from_parts(&header, &enc_buf[..raw_data_len]) {
            info!("[Mesh] BLE->LoRa: portnum={} to={:08x}", portnum, to);
            self.tx_to_lora.send(frame).await;
        }
    }

    /// Send config exchange response to phone's want_config_id request.
    /// Sends: MyNodeInfo, Config{lora}, all active Channels, config_complete_id.
    async fn send_config_exchange(&mut self, config_id: u32) {
        let my_num = self.device.my_node_num;

        // 1. MyNodeInfo
        let id = self.next_from_radio_id();
        let mut tmp = [0u8; 64];
        let len = encode_from_radio_my_info(&mut tmp, id, my_num);
        let mut data = heapless::Vec::<u8, 512>::new();
        data.extend_from_slice(&tmp[..len]).ok();
        self.tx_to_ble.send(FromRadioMessage { data }).await;

        // 2. Config { lora: LoRaConfig }
        let id = self.next_from_radio_id();
        let mut tmp = [0u8; 64];
        let len = encode_from_radio_lora_config(&mut tmp, id);
        let mut data = heapless::Vec::<u8, 512>::new();
        data.extend_from_slice(&tmp[..len]).ok();
        self.tx_to_ble.send(FromRadioMessage { data }).await;

        // 3. Active channels — collect data first to release the borrow on self.device
        struct ChData {
            index: u8,
            psk: [u8; 32],
            psk_len: usize,
            name: [u8; 12],
            name_len: usize,
            role: u8,
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
                    role: ch.role as u8,
                })
                .ok();
        }
        for ch in &channel_data {
            let id = self.next_from_radio_id();
            let mut tmp = [0u8; 128];
            let len = encode_from_radio_channel(
                &mut tmp,
                id,
                ch.index,
                &ch.name[..ch.name_len],
                &ch.psk[..ch.psk_len],
                ch.role,
            );
            let mut data = heapless::Vec::<u8, 512>::new();
            data.extend_from_slice(&tmp[..len]).ok();
            self.tx_to_ble.send(FromRadioMessage { data }).await;
        }

        // 4. config_complete_id (field 7, tag 0x38)
        let id = self.next_from_radio_id();
        let mut tmp = [0u8; 32];
        let len = encode_from_radio_config_complete(&mut tmp, id, config_id);
        let mut data = heapless::Vec::<u8, 512>::new();
        data.extend_from_slice(&tmp[..len]).ok();
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
        let mut data_buf = [0u8; 32];
        let data_len = encode_data_message(
            &mut data_buf,
            5, // ROUTING_APP
            &[],
            request_id,
        );

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
                &mut data_buf[..data_len],
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

        if let Some(frame) = RadioFrame::from_parts(&header, &data_buf[..data_len]) {
            self.tx_to_lora.send(frame).await;
        }
    }
}

// ============================================================================
// Protobuf helpers - minimal encode/decode for Meshtastic protocol
// All field numbers used here fit in single-byte tags (field < 16, wire < 8)
// ============================================================================

/// Decode a varint, returns (value, bytes_consumed)
fn decode_varint(data: &[u8]) -> (u64, usize) {
    let mut val: u64 = 0;
    let mut shift = 0u32;
    let mut n = 0;
    for &b in data.iter().take(10) {
        val |= ((b & 0x7F) as u64) << shift;
        n += 1;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    (val, n)
}

/// Encode varint, return bytes written
fn encode_varint(buf: &mut [u8], mut val: u64) -> usize {
    let mut i = 0;
    loop {
        let byte = (val & 0x7F) as u8;
        val >>= 7;
        if val == 0 {
            buf[i] = byte;
            i += 1;
            break;
        } else {
            buf[i] = byte | 0x80;
            i += 1;
        }
    }
    i
}

/// Write a fixed32 field: tag + 4 bytes LE
fn write_fixed32(buf: &mut [u8], i: &mut usize, field_tag: u8, val: u32) {
    buf[*i] = field_tag;
    *i += 1;
    buf[*i..*i + 4].copy_from_slice(&val.to_le_bytes());
    *i += 4;
}

/// Write a varint field: tag + varint
fn write_varint_field(buf: &mut [u8], i: &mut usize, field_tag: u8, val: u64) {
    buf[*i] = field_tag;
    *i += 1;
    *i += encode_varint(&mut buf[*i..], val);
}

/// Write a length-delimited field: tag + length varint + bytes
fn write_bytes_field(buf: &mut [u8], i: &mut usize, field_tag: u8, data: &[u8]) {
    buf[*i] = field_tag;
    *i += 1;
    *i += encode_varint(&mut buf[*i..], data.len() as u64);
    buf[*i..*i + data.len()].copy_from_slice(data);
    *i += data.len();
}

/// Encode a received LoRa packet as `FromRadio { id, packet: MeshPacket { ... decoded ... } }`
/// Writes into buf (assumed >= 512 bytes), returns bytes written.
#[allow(clippy::too_many_arguments)]
fn encode_from_radio_packet(
    buf: &mut [u8],
    from_radio_id: u32,
    header: &PacketHeader,
    channel_index: u8,
    portnum: u32,
    payload: &[u8],
    snr: i8,
    rssi: i16,
) -> usize {
    // Encode Data sub-message
    let mut data_buf = [0u8; 260];
    let mut di = 0;
    // portnum: field 1, varint
    write_varint_field(&mut data_buf, &mut di, 0x08, portnum as u64);
    // payload: field 2, bytes
    if !payload.is_empty() {
        write_bytes_field(&mut data_buf, &mut di, 0x12, payload);
    }
    let data_len = di;

    // Encode MeshPacket sub-message
    let mut pkt_buf = [0u8; 300];
    let mut pi = 0;
    // from: field 1, fixed32 (tag = 0x0D)
    write_fixed32(&mut pkt_buf, &mut pi, 0x0D, header.sender);
    // to: field 2, fixed32 (tag = 0x15)
    write_fixed32(&mut pkt_buf, &mut pi, 0x15, header.destination);
    // channel: field 3, varint (tag = 0x18)
    write_varint_field(&mut pkt_buf, &mut pi, 0x18, channel_index as u64);
    // decoded: field 4, embedded message (tag = 0x22)
    write_bytes_field(&mut pkt_buf, &mut pi, 0x22, &data_buf[..data_len]);
    // id: field 6, fixed32 (tag = 0x35)
    write_fixed32(&mut pkt_buf, &mut pi, 0x35, header.packet_id);
    // rx_snr: field 8, float (tag = 0x45, wire type 5 = 32-bit)
    let snr_f32: f32 = snr as f32;
    let snr_bytes = snr_f32.to_le_bytes();
    pkt_buf[pi] = 0x45;
    pi += 1;
    pkt_buf[pi..pi + 4].copy_from_slice(&snr_bytes);
    pi += 4;
    // hop_limit: field 9, varint (tag = 0x48)
    write_varint_field(&mut pkt_buf, &mut pi, 0x48, header.hop_limit() as u64);
    // want_ack: field 10, bool/varint (tag = 0x50)
    if header.want_ack() {
        write_varint_field(&mut pkt_buf, &mut pi, 0x50, 1);
    }
    // rx_rssi: field 12, int32/varint (tag = 0x60)
    // Negative values encoded as unsigned 64-bit (int32 wire format)
    write_varint_field(&mut pkt_buf, &mut pi, 0x60, rssi as i64 as u64);
    let pkt_len = pi;

    // Encode FromRadio
    let mut i = 0;
    // id: field 1, varint (tag = 0x08)
    write_varint_field(buf, &mut i, 0x08, from_radio_id as u64);
    // packet: field 2, embedded message (tag = 0x12)
    write_bytes_field(buf, &mut i, 0x12, &pkt_buf[..pkt_len]);

    i
}

/// Encode `FromRadio { id, my_info: MyNodeInfo { my_node_num } }`
/// field 3 in FromRadio = my_info (tag = 0x1A)
fn encode_from_radio_my_info(buf: &mut [u8], from_radio_id: u32, my_node_num: u32) -> usize {
    // MyNodeInfo: my_node_num = field 1 (varint, tag 0x08)
    let mut mi_buf = [0u8; 12];
    let mut mi = 0;
    write_varint_field(&mut mi_buf, &mut mi, 0x08, my_node_num as u64);

    let mut i = 0;
    write_varint_field(buf, &mut i, 0x08, from_radio_id as u64);
    write_bytes_field(buf, &mut i, 0x1A, &mi_buf[..mi]);
    i
}

/// Encode `FromRadio { id, config_complete_id: config_id }`
/// config_complete_id = field 7 in FromRadio (varint, tag = 0x38)
fn encode_from_radio_config_complete(buf: &mut [u8], from_radio_id: u32, config_id: u32) -> usize {
    let mut i = 0;
    write_varint_field(buf, &mut i, 0x08, from_radio_id as u64);
    write_varint_field(buf, &mut i, 0x38, config_id as u64);
    i
}

/// Encode `FromRadio { id, config: Config { lora: LoRaConfig { ... } } }`
/// Config = field 5 in FromRadio (tag 0x2A), Config.lora = field 6 (tag 0x32)
/// LoRaConfig fields: use_preset=1(0x08), modem_preset=2(0x10), region=7(0x38),
///   hop_limit=8(0x40), tx_enabled=9(0x48), tx_power=10(0x50)
/// Region enum: EU_433 = 2, ModemPreset: LongFast = 0
fn encode_from_radio_lora_config(buf: &mut [u8], from_radio_id: u32) -> usize {
    // Encode LoRaConfig inner message
    let mut lora_buf = [0u8; 24];
    let mut li = 0;
    write_varint_field(&mut lora_buf, &mut li, 0x08, 1); // use_preset = true
    write_varint_field(&mut lora_buf, &mut li, 0x10, 0); // modem_preset = LongFast
    write_varint_field(&mut lora_buf, &mut li, 0x38, 2); // region = EU_433
    write_varint_field(&mut lora_buf, &mut li, 0x40, DEFAULT_HOP_LIMIT as u64); // hop_limit
    write_varint_field(&mut lora_buf, &mut li, 0x48, 1); // tx_enabled = true
    write_varint_field(&mut lora_buf, &mut li, 0x50, LORA_TX_POWER_DBM as u64); // tx_power

    // Encode Config message: lora = field 6 (tag 0x32)
    let mut cfg_buf = [0u8; 32];
    let mut ci = 0;
    write_bytes_field(&mut cfg_buf, &mut ci, 0x32, &lora_buf[..li]);

    // Encode FromRadio: id=1 (0x08), config=5 (0x2A)
    let mut i = 0;
    write_varint_field(buf, &mut i, 0x08, from_radio_id as u64);
    write_bytes_field(buf, &mut i, 0x2A, &cfg_buf[..ci]);
    i
}

/// Encode `FromRadio { id, channel: Channel { index, settings: { psk, name }, role } }`
/// Channel = field 10 in FromRadio (tag 0x52)
/// Channel fields: index=1(0x08), settings=2(0x12), role=3(0x18)
/// ChannelSettings fields: psk=2(0x12), name=3(0x1A)
fn encode_from_radio_channel(
    buf: &mut [u8],
    from_radio_id: u32,
    channel_index: u8,
    name: &[u8],
    psk: &[u8],
    role: u8,
) -> usize {
    // Encode ChannelSettings
    let mut settings_buf = [0u8; 48];
    let mut si = 0;
    if !psk.is_empty() {
        write_bytes_field(&mut settings_buf, &mut si, 0x12, psk);
    }
    if !name.is_empty() {
        write_bytes_field(&mut settings_buf, &mut si, 0x1A, name);
    }

    // Encode Channel message
    let mut ch_buf = [0u8; 64];
    let mut ci = 0;
    write_varint_field(&mut ch_buf, &mut ci, 0x08, channel_index as u64);
    write_bytes_field(&mut ch_buf, &mut ci, 0x12, &settings_buf[..si]);
    write_varint_field(&mut ch_buf, &mut ci, 0x18, role as u64);

    // Encode FromRadio: id=1 (0x08), channel=10 (0x52)
    let mut i = 0;
    write_varint_field(buf, &mut i, 0x08, from_radio_id as u64);
    write_bytes_field(buf, &mut i, 0x52, &ch_buf[..ci]);
    i
}

/// Decode a minimal Data protobuf message: portnum (field 1) and payload (field 2)
fn decode_data_message(data: &[u8]) -> Option<(u32, heapless::Vec<u8, 256>)> {
    let mut portnum: u32 = 0;
    let mut payload: heapless::Vec<u8, 256> = heapless::Vec::new();

    let mut i = 0;
    while i < data.len() {
        let tag = data[i];
        i += 1;
        let field = tag >> 3;
        let wire = tag & 0x07;

        match (field, wire) {
            (1, 0) => {
                // portnum: enum/varint
                let (v, n) = decode_varint(&data[i..]);
                i += n;
                portnum = v as u32;
            }
            (2, 2) => {
                // payload: bytes
                let (len, n) = decode_varint(&data[i..]);
                i += n;
                let end = (i + len as usize).min(data.len());
                payload.extend_from_slice(&data[i..end]).ok();
                i = end;
            }
            (_, 0) => {
                let (_, n) = decode_varint(&data[i..]);
                i += n;
            }
            (_, 2) => {
                let (len, n) = decode_varint(&data[i..]);
                i += n;
                i += len as usize;
            }
            (_, 5) => {
                i += 4;
            }
            (_, 1) => {
                i += 8;
            }
            _ => return None,
        }
    }

    if portnum != 0 {
        Some((portnum, payload))
    } else {
        None
    }
}

/// Encode a minimal Data protobuf message
fn encode_data_message(buf: &mut [u8], portnum: u32, payload: &[u8], request_id: u32) -> usize {
    let mut i = 0;

    // field 1: portnum (varint, tag 0x08)
    write_varint_field(buf, &mut i, 0x08, portnum as u64);

    // field 2: payload (bytes, tag 0x12)
    if !payload.is_empty() {
        write_bytes_field(buf, &mut i, 0x12, payload);
    }

    // field 6: request_id (fixed32, tag 0x35)
    if request_id != 0 {
        write_fixed32(buf, &mut i, 0x35, request_id);
    }

    i
}
