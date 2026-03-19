//! Dispatch for packets arriving from the LoRa radio.
//!
//! `dispatch()` is async and performs side effects directly via `MeshCtx`.
//!
//! # How to add a new LoRa portnum handler
//! 1. Create `from_radio/my_portnum.rs` with an async `handle(ctx, ...)` fn
//! 2. Add `pub mod my_portnum;` here
//! 3. Add a match arm: `Some(PortNum::MyPortnum) => my_portnum::handle(ctx, ...).await`
//! 4. The handler may: call `forward_to_ble`, `send_routing_ack`, or update `ctx` state

pub mod neighbor_info;
pub mod node_info;
pub mod position;
pub mod remote_hardware;
pub mod routing;
pub mod telemetry;
pub mod text_message;
pub mod traceroute;
pub mod waypoint;

use crate::domain::context::MeshCtx;
use crate::domain::crypto;
use crate::domain::handlers::util::{forward_to_ble, send_routing_ack};
use crate::domain::packet::{HEADER_SIZE, RadioFrame};
use crate::inter_task::channels::{LedCommand, LedPattern, RadioMetadata};
use crate::ports::MeshStorage;
use crate::proto::{Data, PortNum};
use embassy_time::{Duration, Instant};
use log::{debug, info, warn};
use prost::Message;

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
    if header.sender == ctx.device.my_node_num {
        let idx = ctx
            .pending_acks
            .iter()
            .position(|a| a.packet_id == header.packet_id);
        if let Some(i) = idx {
            info!(
                "[Mesh] Implicit ACK: heard rebroadcast of {:08x}",
                header.packet_id
            );
            ctx.pending_acks.swap_remove(i);
        }
        debug!("[Mesh] Own packet rebroadcast heard, dropping");
        return;
    }

    // Duplicate detection (pass current time so router stays platform-free)
    let now_ms = Instant::now().as_ticks() * 1_000 / embassy_time::TICK_HZ;
    if ctx
        .router
        .is_duplicate(header.sender, header.packet_id, now_ms)
    {
        // P0: Check if this duplicate has a better path (higher hop_limit) than
        // our pending rebroadcast. If so, upgrade the queued copy with the better
        // hop_limit.
        if let Some(pending) = ctx.pending_rebroadcast.as_mut()
            && let Some(pending_hdr) = pending.frame.header()
            && pending_hdr.sender == header.sender
            && pending_hdr.packet_id == header.packet_id
            && header.hop_limit() > pending_hdr.hop_limit()
        {
            // Replace with the better copy
            let new_hop = header.hop_limit() - 1;
            let mut upgraded = frame.clone();
            if let Some(mut hdr) = upgraded.header() {
                hdr.set_hop_limit(new_hop);
                let mut hdr_buf = [0u8; HEADER_SIZE];
                hdr.encode(&mut hdr_buf);
                upgraded.data[..HEADER_SIZE].copy_from_slice(&hdr_buf);
            }
            pending.frame = upgraded;
            info!(
                "[Mesh] Duplicate upgrade: {:08x} hop_limit {} -> {}",
                header.packet_id,
                pending_hdr.hop_limit(),
                new_hop
            );
            return;
        }
        debug!("[Mesh] Duplicate packet, dropping");
        return;
    }

    let _ = ctx
        .led_commands
        .try_send(LedCommand::Blink(LedPattern::SingleBlink));

    // Update NodeDB
    ctx.node_db.touch(header.sender, 0, metadata.snr, now_ms);

    // Try to decrypt
    let preset_name = ctx.device.modem_preset.display_name();
    let channel = ctx
        .device
        .channels
        .find_by_hash(header.channel_index, preset_name);

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

    let addressed_to_us = header.is_for_us(ctx.device.my_node_num);

    // Matching per-portnum handlers
    match PortNum::try_from(portnum).ok() {
        Some(PortNum::RemoteHardwareApp) => {
            remote_hardware::handle(ctx, header.sender, &inner_payload).await;
        }
        Some(PortNum::TextMessageApp | PortNum::TextMessageCompressedApp) => {
            text_message::handle(ctx, &frame, header.sender, &inner_payload).await;
        }
        Some(PortNum::PositionApp) => {
            position::handle(ctx, header.sender, &inner_payload).await;
        }
        Some(PortNum::NodeinfoApp) => {
            node_info::handle(ctx, header.sender, &inner_payload, want_response).await;
        }
        Some(PortNum::RoutingApp) => {
            routing::handle(ctx, header.sender, &inner_payload, request_id).await;
        }
        Some(PortNum::AdminApp) => {
            if addressed_to_us {
                crate::domain::handlers::admin::dispatch(
                    ctx,
                    header.sender,
                    header.packet_id,
                    &inner_payload,
                )
                .await;
            } else {
                // Admin packets for others are forwarded to BLE as normal
                forward_to_ble(
                    ctx,
                    &header,
                    channel_index,
                    portnum,
                    &inner_payload,
                    metadata,
                )
                .await;
            }
        }
        Some(PortNum::WaypointApp) => {
            waypoint::handle(ctx, header.sender, &inner_payload).await;
        }
        Some(PortNum::TelemetryApp) => {
            telemetry::handle(ctx, header.sender, &inner_payload).await;
        }
        Some(PortNum::TracerouteApp) => {
            traceroute::handle(
                ctx,
                header.sender,
                header.packet_id,
                &inner_payload,
                addressed_to_us,
                want_response,
                metadata.snr,
            )
            .await;
        }
        Some(PortNum::NeighborinfoApp) => {
            neighbor_info::handle(ctx, header.sender, &inner_payload).await;
        }
        _ => {
            warn!(
                "[PortHandler] Unknown portnum {} from {:08x}",
                portnum, header.sender
            );
        }
    }

    // Default: Forward to BLE if not AdminApp for us
    if (portnum != PortNum::AdminApp as i32 || !addressed_to_us) && *ctx.ble_connected {
        forward_to_ble(
            ctx,
            &header,
            channel_index,
            portnum,
            &inner_payload,
            metadata,
        )
        .await;
    }

    // Send ACK if addressed to us and want_ack set
    if addressed_to_us && header.want_ack() {
        send_routing_ack(ctx, header.sender, header.packet_id).await;
    }

    // Rebroadcast decision (gated by role)
    if should_rebroadcast_for_role(ctx.device.role)
        && let Some(new_hop) = ctx
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

        let delay = ctx.router.rebroadcast_delay_ms(metadata.snr);
        *ctx.pending_rebroadcast = Some(crate::domain::pending::PendingRebroadcast {
            frame: rebroadcast_frame,
            deadline: Instant::now() + Duration::from_millis(delay),
        });
        debug!("[Mesh] Scheduling rebroadcast in {}ms", delay);
    }
}

fn should_rebroadcast_for_role(role: crate::domain::device::DeviceRole) -> bool {
    use crate::domain::device::DeviceRole;
    !matches!(role, DeviceRole::ClientMute | DeviceRole::ClientHidden)
}
