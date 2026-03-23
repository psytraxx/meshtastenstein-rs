use crate::constants::*;
use crate::domain::context::MeshCtx;
use crate::domain::crypto;
use crate::domain::packet::{PacketHeader, RadioFrame};
use crate::ports::MeshStorage;
use crate::proto::{Data, PortNum, RouteDiscovery};
use log::info;
use prost::Message;

pub async fn handle<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, pkt: &super::InboundPacket<'_>) {
    info!("[PortHandler] Traceroute from {:08x}", pkt.sender);

    // Traceroute reply: append our node_num + SNR, return RouteDiscovery to sender
    if pkt.addressed_to_us && pkt.want_response {
        // Decode existing RouteDiscovery (may be empty for initial request)
        let mut route_disc = RouteDiscovery::decode(pkt.payload).unwrap_or_default();
        // Append our node_num and SNR (SNR scaled by 4 per protocol)
        route_disc.route.push(ctx.device.my_node_num);
        route_disc.snr_towards.push(pkt.snr as i32 * 4);

        let route_bytes = route_disc.encode_to_vec();
        let reply_packet_id = ctx.device.next_packet_id();

        let mut data_bytes = Data {
            portnum: PortNum::TracerouteApp as i32,
            payload: route_bytes,
            request_id: pkt.packet_id,
            ..Default::default()
        }
        .encode_to_vec();

        // Use the same channel the traceroute request arrived on
        let preset_name = ctx.device.modem_preset.display_name();
        let channel = ctx
            .device
            .channels
            .get(pkt.channel_idx)
            .or_else(|| ctx.device.channels.primary());
        let channel_hash = channel.map(|c| c.hash(preset_name)).unwrap_or(0);

        if let Some(ch) = channel
            && ch.is_encrypted()
        {
            let (psk_copy, psk_len) = crypto::copy_psk(ch.effective_psk());
            let _ = crypto::crypt_packet(
                &psk_copy[..psk_len],
                reply_packet_id,
                ctx.device.my_node_num,
                &mut data_bytes,
            );
        }

        let next_hop = ctx.router.get_next_hop(ctx.node_db, pkt.sender, 0);
        let relay_node = (ctx.device.my_node_num & 0xFF) as u8;

        let header = PacketHeader {
            destination: pkt.sender,
            sender: ctx.device.my_node_num,
            packet_id: reply_packet_id,
            flags: PacketHeader::make_flags(false, false, DEFAULT_HOP_LIMIT, DEFAULT_HOP_LIMIT),
            channel_index: channel_hash,
            next_hop,
            relay_node,
        };

        if let Some(frame) = RadioFrame::from_parts(&header, &data_bytes) {
            info!(
                "[Mesh] Traceroute reply to {:08x} with {} hops",
                pkt.sender,
                route_disc.route.len()
            );
            ctx.tx_to_lora.send(frame).await;
        }
    }
}
