//! Mesh task: central orchestrator for Meshtastic protocol
//!
//! Processes LoRa RX -> decrypt -> route -> forward to BLE and/or rebroadcast
//! Processes BLE RX (ToRadio) -> encrypt -> queue for LoRa TX
//! Manages NodeDB updates, rebroadcast timers, config exchange

use crate::constants::*;
use crate::inter_task::channels::{FromRadioMessage, ToRadioMessage};
use crate::mesh::crypto;
use crate::mesh::device::DeviceState;
use crate::mesh::node_db::NodeDB;
use crate::mesh::packet::{HEADER_SIZE, PacketHeader, RadioFrame};
use crate::mesh::portnum_handler::{self, HandleResult};
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
        }
    }

    fn signal_activity(&self) {
        self.activity_signal.signal(Instant::now());
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
                    // Rebroadcast timer fired
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
                    info!("[Mesh] BLE {}", if connected { "connected" } else { "disconnected" });
                    if connected {
                        self.signal_activity();
                    }
                }
                Either::Second(Either4::Fourth(_)) => {
                    let _ = self.led_commands.try_send(LedCommand::Blink(LedPattern::Heartbeat));
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

        let _ = self.led_commands.try_send(LedCommand::Blink(LedPattern::SingleBlink));

        // Update NodeDB
        self.node_db.touch(header.sender, 0, metadata.snr);

        // Try to decrypt
        let channel = self.device.channels.find_by_hash(header.channel_index);
        let mut payload = frame.payload().to_vec();

        if let Some(ch) = channel
            && ch.is_encrypted() && !payload.is_empty()
        {
            let psk = ch.effective_psk();
            if crypto::crypt_packet(psk, header.packet_id, header.sender, &mut payload).is_err() {
                warn!("[Mesh] Decryption failed for channel {}", header.channel_index);
                return;
            }
        }

        // Try to decode as protobuf Data message
        // The decrypted payload is a protobuf-encoded `Data` message
        // We do minimal inline decoding to extract portnum and payload
        if let Some((portnum, inner_payload)) = decode_data_message(&payload) {
            let result = portnum_handler::handle_portnum(
                portnum,
                &inner_payload,
                header.sender,
                &mut self.node_db,
                0, // TODO: real time
            );

            match result {
                HandleResult::TextMessage(_text_data) => {
                    // Forward to BLE
                    if self.ble_connected {
                        let mut data = heapless::Vec::new();
                        // Re-encode as FromRadio message for the phone
                        // For now, forward raw frame data
                        data.extend_from_slice(frame.as_bytes()).ok();
                        let msg = FromRadioMessage { data };
                        let _ = self.tx_to_ble.try_send(msg);
                    }

                    // Send ACK if addressed to us and want_ack
                    if header.is_for_us(self.device.my_node_num) && header.want_ack() {
                        self.send_routing_ack(header.sender, header.packet_id).await;
                    }
                }
                HandleResult::RoutingResponse(dest, _req_id, _error_code) => {
                    debug!("[Mesh] Routing response: dest={:08x}", dest);
                }
                HandleResult::Handled => {}
                HandleResult::NotHandled => {}
            }
        }

        // Forward full frame to BLE regardless (phone app does its own decoding)
        if self.ble_connected {
            let mut data = heapless::Vec::new();
            data.extend_from_slice(frame.as_bytes()).ok();
            let msg = FromRadioMessage { data };
            let _ = self.tx_to_ble.try_send(msg);
        }

        // Rebroadcast decision
        if let Some(new_hop) = self.router.should_rebroadcast(header.hop_limit(), header.sender) {
            let mut rebroadcast_frame = frame.clone();
            // Update hop limit in the frame
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
        debug!("[Mesh] BLE->Radio: {} bytes", msg.data.len());

        // TODO: Decode ToRadio protobuf message
        // For now, if it contains a MeshPacket, encode and transmit
        // The phone sends ToRadio { packet: MeshPacket { ... } }

        // Placeholder: treat raw data as a mesh packet to transmit
        // Real implementation would decode ToRadio protobuf first
        let _ = self.led_commands.try_send(LedCommand::Blink(LedPattern::DoubleBlink));

        // For now, just log it
        info!("[Mesh] Received ToRadio message ({} bytes), TODO: decode and transmit", msg.data.len());
    }

    /// Send a routing ACK for a received packet
    async fn send_routing_ack(&mut self, dest: u32, request_id: u32) {
        debug!("[Mesh] Sending ACK to {:08x} for packet {:08x}", dest, request_id);

        // Build routing ACK payload (protobuf Routing message with error_reason = NONE)
        let ack_payload = [0u8; 8];
        // Field 2 (error_reason) = 0 (NONE), varint encoding = 0x10 0x00
        // But Routing message uses request_id in Data.request_id, not in payload
        let ack_payload_len = 0; // Empty routing payload = ACK success

        // Build Data protobuf
        let mut data_buf = [0u8; 32];
        let data_len = encode_data_message(
            &mut data_buf,
            5, // ROUTING_APP
            &ack_payload[..ack_payload_len],
            request_id,
        );

        // Get packet ID first (mutable borrow)
        let packet_id = self.device.next_packet_id();

        // Then encrypt using channel PSK (immutable borrow)
        if let Some(ch) = self.device.channels.primary()
            && ch.is_encrypted()
        {
            let mut psk_copy = [0u8; 32];
            let psk = ch.effective_psk();
            let psk_len = psk.len();
            psk_copy[..psk_len].copy_from_slice(psk);
            let _ = crypto::crypt_packet(&psk_copy[..psk_len], packet_id, self.device.my_node_num, &mut data_buf[..data_len]);
        }

        // Build header
        let header = PacketHeader {
            destination: dest,
            sender: self.device.my_node_num,
            packet_id,
            flags: PacketHeader::make_flags(false, false, DEFAULT_HOP_LIMIT, DEFAULT_HOP_LIMIT),
            channel_index: 0,
            next_hop: 0,
            relay_node: 0,
        };

        if let Some(frame) = RadioFrame::from_parts(&header, &data_buf[..data_len]) {
            self.tx_to_lora.send(frame).await;
        }
    }
}

/// Minimal decode of protobuf Data message to extract portnum and payload
/// Data proto fields: portnum (field 1, enum/varint), payload (field 2, bytes)
fn decode_data_message(data: &[u8]) -> Option<(u32, heapless::Vec<u8, 256>)> {
    let mut portnum: u32 = 0;
    let mut payload: heapless::Vec<u8, 256> = heapless::Vec::new();

    let mut i = 0;
    while i < data.len() {
        let tag_byte = data[i];
        i += 1;
        let field_num = tag_byte >> 3;
        let wire_type = tag_byte & 0x07;

        match (field_num, wire_type) {
            // portnum: enum (field 1, varint)
            (1, 0) => {
                let mut val: u32 = 0;
                let mut shift = 0;
                while i < data.len() {
                    let b = data[i];
                    i += 1;
                    val |= ((b & 0x7F) as u32) << shift;
                    if b & 0x80 == 0 {
                        break;
                    }
                    shift += 7;
                }
                portnum = val;
            }
            // payload: bytes (field 2, length-delimited)
            (2, 2) => {
                let mut len: usize = 0;
                let mut shift = 0;
                while i < data.len() {
                    let b = data[i];
                    i += 1;
                    len |= ((b & 0x7F) as usize) << shift;
                    if b & 0x80 == 0 {
                        break;
                    }
                    shift += 7;
                }
                if i + len <= data.len() {
                    payload.extend_from_slice(&data[i..i + len]).ok();
                    i += len;
                } else {
                    return None;
                }
            }
            // Skip other fields
            (_, 0) => {
                while i < data.len() {
                    if data[i] & 0x80 == 0 {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            (_, 1) => i += 8,
            (_, 2) => {
                let mut len: usize = 0;
                let mut shift = 0;
                while i < data.len() {
                    let b = data[i];
                    i += 1;
                    len |= ((b & 0x7F) as usize) << shift;
                    if b & 0x80 == 0 {
                        break;
                    }
                    shift += 7;
                }
                i += len;
            }
            (_, 5) => i += 4,
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
/// Returns number of bytes written
fn encode_data_message(buf: &mut [u8], portnum: u32, payload: &[u8], request_id: u32) -> usize {
    let mut i = 0;

    // Field 1: portnum (varint)
    buf[i] = 1 << 3; // field 1, wire type 0 (varint)
    i += 1;
    i += encode_varint(&mut buf[i..], portnum as u64);

    // Field 2: payload (bytes)
    if !payload.is_empty() {
        buf[i] = (2 << 3) | 2; // field 2, wire type 2
        i += 1;
        i += encode_varint(&mut buf[i..], payload.len() as u64);
        buf[i..i + payload.len()].copy_from_slice(payload);
        i += payload.len();
    }

    // Field 6: request_id (fixed32)
    if request_id != 0 {
        buf[i] = (6 << 3) | 5; // field 6, wire type 5
        i += 1;
        buf[i..i + 4].copy_from_slice(&request_id.to_le_bytes());
        i += 4;
    }

    i
}

/// Encode a varint, return bytes written
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
