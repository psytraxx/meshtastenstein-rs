use crate::{domain::context::MeshCtx, ports::MeshStorage, proto::Telemetry};
use log::{info, warn};
use prost::Message;

pub async fn handle<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, pkt: &super::InboundPacket<'_>) {
    let telemetry = match Telemetry::decode(pkt.payload) {
        Ok(t) => t,
        Err(e) => {
            warn!(
                "[PortHandler] Telemetry decode failed from {:08x}: {:?}",
                pkt.sender, e
            );
            return;
        }
    };

    // Update NodeDB with device metrics if present
    if let Some(crate::proto::telemetry::Variant::DeviceMetrics(metrics)) = &telemetry.variant {
        // Store them. This is upstream's `nodeTelemetry` satellite map: the
        // metrics are NOT bundled into NodeInfo (other-node NodeInfos go out
        // thin), they are replayed to the phone as synthesised TELEMETRY_APP
        // packets after config-complete. Logging alone left nothing to replay.
        ctx.node_db.update_device_metrics(pkt.sender, *metrics);
        info!(
            "[PortHandler] Telemetry from {:08x}: bat={:?}% voltage={:?}V ch_util={:?}% air_tx={:?}%",
            pkt.sender,
            metrics.battery_level,
            metrics.voltage,
            metrics.channel_utilization,
            metrics.air_util_tx,
        );
    } else {
        info!(
            "[PortHandler] Telemetry from {:08x} (non-device variant)",
            pkt.sender
        );
    }
    // BLE forwarding is handled by the central dispatch in from_radio/mod.rs
}
