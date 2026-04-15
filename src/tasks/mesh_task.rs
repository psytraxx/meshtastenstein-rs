//! Mesh task: central orchestrator for Meshtastic protocol
//!
//! Simplified event loop that dispatches to async domain handlers.

extern crate alloc;
use crate::{
    constants::*,
    domain::{
        context::{ChannelMetrics, MeshCtx},
        device::DeviceState,
        handlers,
        node_db::NodeDB,
        router::{MeshRouter, PendingPacket, PendingRebroadcast},
    },
    inter_task::channels::{Channels, LedCommand, LedPattern, MeshEvent},
    ports::MeshStorage,
};
use embassy_futures::select::{Either, Either3, select, select3};
use embassy_time::{Duration, Instant, Ticker, Timer};
use log::{debug, info};

/// Minimum spacing between NodeDB snapshot flushes.
const NODE_DB_FLUSH_INTERVAL_MS: u64 = 5 * 60 * 1000;

/// All mutable mesh state, grouped so `MeshOrchestrator` avoids listing
/// every field three times (struct, `new`, `make_ctx`).
///
/// `make_ctx()` borrows fields from here to build `MeshCtx`; handler code
/// is unchanged — it still accesses `ctx.device`, `ctx.node_db`, etc.
struct MeshState<S: 'static> {
    device: DeviceState,
    node_db: NodeDB,
    storage: &'static mut S,
    router: MeshRouter,
    pending_packets: heapless::Vec<PendingPacket, 8>,
    pending_rebroadcast: Option<PendingRebroadcast>,
    my_position_bytes: heapless::Vec<u8, 64>,
    session_passkey: Option<[u8; 16]>,
    from_radio_id: u32,
    ble_connected: bool,
    last_nodeinfo_tx: Option<Instant>,
    last_position_tx: Instant,
    last_lora_telemetry: Option<Instant>,
    last_neighborinfo_tx: Option<Instant>,
    channel_metrics: ChannelMetrics,
    reboot_after_secs: Option<u32>,
    shutdown_after_secs: Option<u32>,
    node_id_str: alloc::string::String,
    boot_time: Instant,
    pkc_priv_bytes: [u8; 32],
    pkc_pub_bytes: [u8; 32],
    /// Debounced NodeDB flush: last time we successfully wrote to flash.
    last_node_db_flush: Instant,
}

impl<S: MeshStorage> MeshState<S> {
    fn new(mac: &[u8; 6], storage: &'static mut S, pkc_keypair: ([u8; 32], [u8; 32])) -> Self {
        let mut device = DeviceState::new(mac);
        let node_num = device.my_node_num;

        storage.load_state(&mut device);

        // Restore mesh state from the previous session (NodeDB snapshot).
        let mut node_db = NodeDB::new(node_num);
        storage.load_node_db(&mut node_db);

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

        Self {
            node_id_str: handlers::util::build_node_id_string(node_num),
            router: MeshRouter::new(node_num),
            device,
            node_db,
            storage,
            pending_rebroadcast: None,
            ble_connected: false,
            from_radio_id: 1,
            session_passkey: None,
            pending_packets: heapless::Vec::new(),
            my_position_bytes: heapless::Vec::new(),
            last_position_tx: Instant::now(),
            last_lora_telemetry: None,
            boot_time: Instant::now(),
            last_nodeinfo_tx: None,
            channel_metrics: ChannelMetrics::default(),
            last_neighborinfo_tx: None,
            reboot_after_secs: None,
            shutdown_after_secs: None,
            last_node_db_flush: Instant::now(),
            pkc_priv_bytes: pkc_keypair.0,
            pkc_pub_bytes: pkc_keypair.1,
        }
    }
}

/// Central mesh orchestrator — thin event-pump wrapper around `MeshState`.
pub struct MeshOrchestrator<S: 'static> {
    channels: &'static Channels,
    state: MeshState<S>,
}

impl<S: MeshStorage> MeshOrchestrator<S> {
    pub fn new(
        channels: &'static Channels,
        mac: &[u8; 6],
        storage: &'static mut S,
        pkc_keypair: ([u8; 32], [u8; 32]),
    ) -> Self {
        Self {
            channels,
            state: MeshState::new(mac, storage, pkc_keypair),
        }
    }

    fn make_ctx(&mut self) -> MeshCtx<'_, S> {
        let s = &mut self.state;
        MeshCtx {
            device: &mut s.device,
            node_db: &mut s.node_db,
            storage: s.storage,
            router: &mut s.router,
            pending_packets: &mut s.pending_packets,
            pending_rebroadcast: &mut s.pending_rebroadcast,
            my_position_bytes: &mut s.my_position_bytes,
            session_passkey: &mut s.session_passkey,
            from_radio_id: &mut s.from_radio_id,
            ble_connected: &mut s.ble_connected,
            last_nodeinfo_tx: &mut s.last_nodeinfo_tx,
            last_position_tx: &mut s.last_position_tx,
            last_lora_telemetry: &mut s.last_lora_telemetry,
            last_neighborinfo_tx: &mut s.last_neighborinfo_tx,
            channel_metrics: &mut s.channel_metrics,
            reboot_after_secs: &mut s.reboot_after_secs,
            shutdown_after_secs: &mut s.shutdown_after_secs,
            node_id_str: s.node_id_str.as_str(),
            boot_time: s.boot_time,
            pkc_pub_bytes: &s.pkc_pub_bytes,
            pkc_priv_bytes: &s.pkc_priv_bytes,
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

            // Debounced NodeDB flush: write at most once every NODE_DB_FLUSH_INTERVAL_MS.
            if self.state.node_db.is_dirty()
                && self.state.last_node_db_flush.elapsed()
                    >= Duration::from_millis(NODE_DB_FLUSH_INTERVAL_MS)
            {
                self.state.storage.save_node_db(&self.state.node_db);
                self.state.node_db.mark_clean();
                self.state.last_node_db_flush = Instant::now();
            }

            // Deferred shutdown: forward to watchdog (which owns DeepSleepAdapter).
            if let Some(secs) = self.state.shutdown_after_secs.take() {
                info!(
                    "[Mesh] Shutdown requested in {} seconds — handing off to watchdog",
                    secs
                );
                // Final flush before the radio goes dark.
                if self.state.node_db.is_dirty() {
                    self.state.storage.save_node_db(&self.state.node_db);
                    self.state.node_db.mark_clean();
                }
                self.channels.shutdown_cmd.signal(secs);
            }

            // Deferred reboot: admin handlers set this; we reset after dispatch.
            if let Some(secs) = self.state.reboot_after_secs.take() {
                info!("[Mesh] Rebooting in {} seconds (admin request)", secs);
                Timer::after(Duration::from_secs(secs as u64)).await;
                esp_hal::system::software_reset();
            }
        }
    }

    async fn next_event(&mut self, heartbeat: &mut Ticker) -> MeshEvent {
        loop {
            // Rebroadcast timer
            let rebroadcast_fut = async {
                match self.state.pending_rebroadcast {
                    Some(ref p) => Timer::at(p.deadline).await,
                    None => core::future::pending::<()>().await,
                }
            };

            // Retransmission timer
            let retx_timeout_fut = async {
                match self.state.pending_packets.iter().map(|a| a.deadline).min() {
                    Some(deadline) => Timer::at(deadline).await,
                    None => core::future::pending::<()>().await,
                }
            };

            match select3(
                self.channels.mesh_in.receive(),
                select(rebroadcast_fut, retx_timeout_fut),
                heartbeat.next(),
            )
            .await
            {
                Either3::First(event) => {
                    match &event {
                        MeshEvent::LoraRx(_, meta) => {
                            self.channels.activity.signal(Instant::now());
                            self.channels.radio_stats.signal((meta.rssi, meta.snr));
                            MeshRouter::extend_pending_deadlines(
                                &mut self.state.pending_packets,
                                Duration::from_millis(RETX_AIRTIME_EXTENSION_MS),
                            );
                        }
                        MeshEvent::BleRx(_) => {
                            self.channels.activity.signal(Instant::now());
                        }
                        _ => {}
                    }
                    return event;
                }
                Either3::Second(Either::First(_)) => {
                    if let Some(pending) = self.state.pending_rebroadcast.take() {
                        debug!("[Mesh] Sending rebroadcast");
                        self.channels.lora_tx.send(pending.frame).await;
                    }
                }
                Either3::Second(Either::Second(_)) => {
                    let frames = self.state.router.tick_retransmissions(
                        &mut self.state.pending_packets,
                        &mut self.state.node_db,
                    );
                    for frame in frames {
                        self.channels.lora_tx.send(frame).await;
                    }
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
}
