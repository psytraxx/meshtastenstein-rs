use crate::constants::*;
use crate::domain::context::MeshCtx;
use crate::domain::crypto;
use crate::domain::packet::{PacketHeader, RadioFrame};
use crate::ports::MeshStorage;
use crate::proto::{Data, PortNum, RouteDiscovery};
use log::info;
use prost::Message;

pub async fn handle<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    sender: u32,
    packet_id: u32,
    payload: &[u8],
    addressed_to_us: bool,
    want_response: bool,
    snr: i8,
) {
    info!("[PortHandler] Traceroute from {:08x}", sender);

    // Traceroute reply: append our node_num + SNR, return RouteDiscovery to sender
    if addressed_to_us && want_response {
        // Decode existing RouteDiscovery (may be empty for initial request)
        let mut route_disc = RouteDiscovery::decode(payload).unwrap_or_default();
        // Append our node_num and SNR (SNR scaled by 4 per protocol)
        route_disc.route.push(ctx.device.my_node_num);
        route_disc.snr_towards.push(snr as i32 * 4);

        let route_bytes = route_disc.encode_to_vec();
        let reply_packet_id = ctx.device.next_packet_id();

        let mut data_bytes = Data {
            portnum: PortNum::TracerouteApp as i32,
            payload: route_bytes,
            request_id: packet_id,
            ..Default::default()
        }
        .encode_to_vec();

        let preset_name = ctx.device.modem_preset.display_name();
        let channel = ctx.device.channels.primary();
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
                reply_packet_id,
                ctx.device.my_node_num,
                &mut data_bytes,
            );
        }

        let header = PacketHeader {
            destination: sender,
            sender: ctx.device.my_node_num,
            packet_id: reply_packet_id,
            flags: PacketHeader::make_flags(false, false, DEFAULT_HOP_LIMIT, DEFAULT_HOP_LIMIT),
            channel_index: channel_hash,
            next_hop: 0,
            relay_node: 0,
        };

        if let Some(frame) = RadioFrame::from_parts(&header, &data_bytes) {
            info!(
                "[Mesh] Traceroute reply to {:08x} with {} hops",
                sender,
                route_disc.route.len()
            );
            ctx.tx_to_lora.send(frame).await;
        }
    }
}
