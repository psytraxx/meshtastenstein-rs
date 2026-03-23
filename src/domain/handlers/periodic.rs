use crate::constants::*;
use crate::domain::context::MeshCtx;
use crate::domain::device::DeviceRole;
use crate::domain::handlers::outgoing;
use crate::domain::handlers::util::{encode_from_radio, lora_send, next_from_radio_id};
use crate::domain::packet::BROADCAST_ADDR;
use crate::inter_task::channels::FromRadioMessage;
use crate::ports::MeshStorage;
use crate::proto::{Data, MeshPacket, Neighbor, NeighborInfo, PortNum, from_radio, mesh_packet};
use embassy_time::{Duration, Instant};
use log::{debug, info, warn};
use prost::Message;

pub async fn dispatch<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>) {
    // Periodic NodeInfo re-broadcast
    let nodeinfo_interval = nodeinfo_interval_ms(ctx);
    if nodeinfo_interval > 0 {
        let last = ctx.last_nodeinfo_tx.unwrap_or(Instant::MIN);
        if last.elapsed() >= Duration::from_millis(nodeinfo_interval) {
            broadcast_nodeinfo(ctx).await;
        }
    }

    // Periodic NeighborInfo broadcast (every 6 hours)
    let ni_due = ctx
        .last_neighborinfo_tx
        .map(|t| t.elapsed() >= Duration::from_millis(NEIGHBORINFO_BROADCAST_INTERVAL_MS))
        .unwrap_or(
            // First broadcast after 6 hours from boot
            ctx.boot_time.elapsed() >= Duration::from_millis(NEIGHBORINFO_BROADCAST_INTERVAL_MS),
        );
    if ni_due && ctx.channel_metrics.channel_util < CHANNEL_UTIL_THRESHOLD {
        broadcast_neighborinfo(ctx).await;
    }

    // M6: Periodic position re-broadcast (gated by channel utilization)
    let pos_interval = position_interval_ms(ctx);
    if pos_interval > 0
        && ctx.channel_metrics.channel_util < CHANNEL_UTIL_THRESHOLD
        && !ctx.my_position_bytes.is_empty()
        && ctx.last_position_tx.elapsed() >= Duration::from_millis(pos_interval)
    {
        broadcast_position(ctx).await;
    }
}

pub async fn broadcast_nodeinfo<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>) {
    let payload = outgoing::node_info::build_payload(ctx.device, ctx.node_id_str);
    if lora_send(
        ctx,
        PortNum::NodeinfoApp.into(),
        payload,
        BROADCAST_ADDR,
        false,
    )
    .await
    {
        info!(
            "[Mesh] NodeInfo broadcast: {} ({})",
            ctx.device.long_name.as_str(),
            ctx.device.short_name.as_str()
        );
        *ctx.last_nodeinfo_tx = Some(Instant::now());
    }
}

pub async fn broadcast_position<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>) {
    if ctx.my_position_bytes.is_empty() {
        return;
    }
    let payload = ctx.my_position_bytes.as_slice().to_vec();
    if lora_send(
        ctx,
        PortNum::PositionApp.into(),
        payload,
        BROADCAST_ADDR,
        false,
    )
    .await
    {
        info!("[Mesh] Broadcasting position to mesh");
        *ctx.last_position_tx = Instant::now();
    }
}

pub async fn broadcast_neighborinfo<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>) {
    let mut neighbors = alloc::vec::Vec::new();
    for entry in ctx.node_db.iter() {
        if entry.node_num == ctx.device.my_node_num {
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
        *ctx.last_neighborinfo_tx = Some(Instant::now());
        return;
    }

    let neighbor_count = neighbors.len();
    let ni = NeighborInfo {
        node_id: ctx.device.my_node_num,
        last_sent_by_id: ctx.device.my_node_num,
        node_broadcast_interval_secs: (NEIGHBORINFO_BROADCAST_INTERVAL_MS / 1000) as u32,
        neighbors,
    };
    let ni_bytes = ni.encode_to_vec();

    if lora_send(
        ctx,
        PortNum::NeighborinfoApp.into(),
        ni_bytes,
        BROADCAST_ADDR,
        false,
    )
    .await
    {
        info!(
            "[Mesh] NeighborInfo broadcast: {} neighbor(s)",
            neighbor_count
        );
    }
    *ctx.last_neighborinfo_tx = Some(Instant::now());
}

pub async fn send_device_telemetry<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    battery_level: u8,
    voltage_mv: u16,
) {
    let voltage_v = voltage_mv as f32 / 1000.0;
    let payload = outgoing::telemetry::build_payload(
        battery_level,
        voltage_v,
        ctx.channel_metrics.channel_util,
        ctx.channel_metrics.air_util_tx,
        ctx.boot_time.elapsed().as_secs() as u32,
    );

    // --- LoRa broadcast (rate-limited) ---
    let telemetry_interval = telemetry_interval_ms(ctx);
    let lora_due = telemetry_interval > 0
        && ctx.channel_metrics.channel_util < CHANNEL_UTIL_THRESHOLD
        && ctx
            .last_lora_telemetry
            .map(|t| t.elapsed() >= Duration::from_millis(telemetry_interval))
            .unwrap_or(true);

    if lora_due
        && lora_send(
            ctx,
            PortNum::TelemetryApp.into(),
            payload.clone(),
            BROADCAST_ADDR,
            false,
        )
        .await
    {
        info!(
            "[Mesh] Telemetry LoRa broadcast: battery={}% voltage={:.2}V",
            battery_level, voltage_v
        );
        *ctx.last_lora_telemetry = Some(Instant::now());
    }

    // --- BLE forward (if connected) ---
    if *ctx.ble_connected {
        let packet_id = ctx.device.next_packet_id();
        let from_radio_id = next_from_radio_id(ctx.from_radio_id);
        let data = encode_from_radio(
            from_radio_id,
            from_radio::PayloadVariant::Packet(MeshPacket {
                from: ctx.device.my_node_num,
                to: BROADCAST_ADDR,
                id: packet_id,
                payload_variant: Some(mesh_packet::PayloadVariant::Decoded(Data {
                    portnum: PortNum::TelemetryApp.into(),
                    payload,
                    ..Default::default()
                })),
                ..Default::default()
            }),
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

fn congestion_scale(ctx: &MeshCtx<'_, impl MeshStorage>) -> f32 {
    let now_ms = Instant::now().as_ticks() * 1_000 / embassy_time::TICK_HZ;
    const TWO_HOURS_MS: u64 = 2 * 60 * 60 * 1_000;
    let n = ctx.node_db.online_count(now_ms, TWO_HOURS_MS);
    match n {
        0..=10 => 0.6,
        11..=20 => 0.7,
        21..=30 => 0.8,
        31..=40 => 1.0,
        _ => 1.0 + (n - 40) as f32 * 0.075,
    }
}

#[allow(deprecated)] // Repeater and RouterClient are deprecated in proto but still need handling
fn role_scaled_interval_ms(role: DeviceRole, base_ms: u64, scale: f32) -> u64 {
    match role {
        DeviceRole::Repeater | DeviceRole::ClientHidden => 0,
        DeviceRole::Router | DeviceRole::RouterClient => ROUTER_BROADCAST_INTERVAL_MS,
        _ => (base_ms as f32 * scale) as u64,
    }
}

fn nodeinfo_interval_ms(ctx: &MeshCtx<'_, impl MeshStorage>) -> u64 {
    role_scaled_interval_ms(
        ctx.device.role,
        NODEINFO_BROADCAST_INTERVAL_MS,
        congestion_scale(ctx),
    )
}

fn position_interval_ms(ctx: &MeshCtx<'_, impl MeshStorage>) -> u64 {
    role_scaled_interval_ms(
        ctx.device.role,
        POSITION_BROADCAST_INTERVAL_MS,
        congestion_scale(ctx),
    )
}

fn telemetry_interval_ms(ctx: &MeshCtx<'_, impl MeshStorage>) -> u64 {
    role_scaled_interval_ms(
        ctx.device.role,
        TELEMETRY_LORA_INTERVAL_MS,
        congestion_scale(ctx),
    )
}
