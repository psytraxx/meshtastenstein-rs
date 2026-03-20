//! Mesh task: central orchestrator for Meshtastic protocol
//!
//! Simplified event loop that dispatches to async domain handlers.

extern crate alloc;
use crate::constants::*;
use crate::domain::context::{ChannelMetrics, MeshCtx};
use crate::domain::device::DeviceState;
use crate::domain::handlers;
use crate::domain::node_db::NodeDB;
use crate::domain::pending::{PendingAck, PendingRebroadcast};
use crate::domain::router::MeshRouter;
use crate::inter_task::channels::{Channels, LedCommand, LedPattern, MeshEvent};
use crate::ports::MeshStorage;
use embassy_futures::select::{Either, Either3, select, select3};
use embassy_time::{Duration, Instant, Ticker, Timer};
use log::{debug, info};

/// Central mesh orchestrator
pub struct MeshOrchestrator<S: 'static> {
    channels: &'static Channels,

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
    session_passkey: Option<[u8; 16]>,

    // Flash config persistence
    storage: &'static mut S,

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

    // Channel utilization metrics (updated by lora_task via signal)
    channel_metrics: ChannelMetrics,

    // Last time we broadcast NeighborInfo
    last_neighborinfo_tx: Option<Instant>,
}

impl<S: MeshStorage> MeshOrchestrator<S> {
    pub fn new(channels: &'static Channels, mac: &[u8; 6], storage: &'static mut S) -> Self {
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

        let node_id_str = handlers::util::build_node_id_string(node_num);

        Self {
            channels,
            node_db: NodeDB::new(node_num),
            router: MeshRouter::new(node_num),
            device,
            pending_rebroadcast: None,
            ble_connected: false,
            from_radio_id: 1,
            session_passkey: None,
            storage,
            pending_acks: heapless::Vec::new(),
            my_position_bytes: heapless::Vec::new(),
            last_position_tx: Instant::now(),
            node_id_str,
            last_lora_telemetry: None,
            boot_time: Instant::now(),
            last_nodeinfo_tx: None,
            channel_metrics: ChannelMetrics::default(),
            last_neighborinfo_tx: None,
        }
    }

    fn make_ctx(&mut self) -> MeshCtx<'_, S> {
        MeshCtx {
            device: &mut self.device,
            node_db: &mut self.node_db,
            storage: self.storage,
            router: &mut self.router,
            pending_acks: &mut self.pending_acks,
            pending_rebroadcast: &mut self.pending_rebroadcast,
            my_position_bytes: &mut self.my_position_bytes,
            session_passkey: &mut self.session_passkey,
            from_radio_id: &mut self.from_radio_id,
            ble_connected: &mut self.ble_connected,
            last_nodeinfo_tx: &mut self.last_nodeinfo_tx,
            last_position_tx: &mut self.last_position_tx,
            last_lora_telemetry: &mut self.last_lora_telemetry,
            last_neighborinfo_tx: &mut self.last_neighborinfo_tx,
            channel_metrics: &mut self.channel_metrics,
            node_id_str: self.node_id_str.as_str(),
            boot_time: self.boot_time,
            tx_to_ble: self.channels.ble_tx.sender(),
            tx_to_lora: self.channels.lora_tx.sender(),
            led_commands: self.channels.led_cmd.sender(),
        }
    }

    /// Run the mesh orchestrator loop
    pub async fn run(&mut self) -> ! {
        info!("[Mesh] Starting mesh orchestrator loop...");

        // Announce ourselves on the mesh shortly after boot
        Timer::after(Duration::from_millis(NODEINFO_BOOT_DELAY_MS)).await;
        {
            let mut ctx = self.make_ctx();
            handlers::periodic::broadcast_nodeinfo(&mut ctx).await;
        }

        let mut ticker = Ticker::every(Duration::from_millis(LED_HEARTBEAT_INTERVAL_MS));

        loop {
            let event = self.next_event(&mut ticker).await;
            let mut ctx = self.make_ctx();
            handlers::dispatch(event, &mut ctx).await;
        }
    }

    async fn next_event(&mut self, heartbeat: &mut Ticker) -> MeshEvent {
        loop {
            // Rebroadcast timer
            let rebroadcast_fut = async {
                match self.pending_rebroadcast {
                    Some(ref p) => Timer::at(p.deadline).await,
                    None => core::future::pending::<()>().await,
                }
            };

            // ACK timeout timer (M1)
            let ack_timeout_fut = async {
                match self.pending_acks.iter().map(|a| a.deadline).min() {
                    Some(deadline) => Timer::at(deadline).await,
                    None => core::future::pending::<()>().await,
                }
            };

            match select3(
                self.channels.mesh_in.receive(),
                select(rebroadcast_fut, ack_timeout_fut),
                heartbeat.next(),
            )
            .await
            {
                Either3::First(event) => {
                    // Side effects for specific event types
                    match &event {
                        MeshEvent::LoraRx(_, meta) => {
                            self.channels.activity.signal(Instant::now());
                            self.channels.radio_stats.signal((meta.rssi, meta.snr));
                        }
                        MeshEvent::BleRx(_) => {
                            self.channels.activity.signal(Instant::now());
                        }
                        _ => {}
                    }
                    return event;
                }
                Either3::Second(Either::First(_)) => {
                    if let Some(pending) = self.pending_rebroadcast.take() {
                        debug!("[Mesh] Sending rebroadcast");
                        self.channels.lora_tx.send(pending.frame).await;
                    }
                }
                Either3::Second(Either::Second(_)) => {
                    self.check_ack_timeouts().await;
                }
                Either3::Third(_) => {
                    let _ = self
                        .channels
                        .led_cmd
                        .try_send(LedCommand::Blink(LedPattern::Heartbeat));
                    return MeshEvent::Tick;
                }
            }
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
                    self.channels.lora_tx.send(frame).await;
                    self.pending_acks[i].deadline =
                        Instant::now() + Duration::from_millis(WANT_ACK_TIMEOUT_MS);
                    self.pending_acks[i].retries_left = retries_left;
                    i += 1;
                } else {
                    let packet_id = self.pending_acks[i].packet_id;
                    let dest = self.pending_acks[i].dest;
                    info!(
                        "[Mesh] ACK timeout for {:08x} to {:08x}, giving up",
                        packet_id, dest
                    );
                    self.pending_acks.swap_remove(i);
                }
            } else {
                i += 1;
            }
        }
    }
}

