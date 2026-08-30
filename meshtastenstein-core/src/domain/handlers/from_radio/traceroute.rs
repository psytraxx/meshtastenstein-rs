use crate::{
    domain::{context::MeshCtx, tx::TxBuilder},
    ports::MeshStorage,
    proto::{PortNum, RouteDiscovery},
};
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
        let hop_count = route_disc.route.len();

        let route_bytes = route_disc.encode_to_vec();
        let reply_packet_id = ctx.device.next_packet_id();

        // Reply on the same channel the traceroute request arrived on.
        let frame = (TxBuilder {
            dest: pkt.sender,
            portnum: PortNum::TracerouteApp.into(),
            inner_payload: route_bytes,
            channel_idx: Some(pkt.channel_idx),
            request_id: pkt.packet_id,
            ..Default::default()
        })
        .build(ctx.device, ctx.router, ctx.node_db, reply_packet_id, None);

        if let Some(frame) = frame {
            info!(
                "[Mesh] Traceroute reply to {:08x} with {} hops",
                pkt.sender, hop_count
            );
            ctx.tx_to_lora.send(frame).await;
        }
    }
}
