extern crate alloc;
use crate::domain::device::DeviceState;
use crate::domain::node_db::NodeDB;
use crate::domain::packet::RadioFrame;
use crate::domain::pending::{PendingAck, PendingRebroadcast};
use crate::domain::router::MeshRouter;
use crate::inter_task::channels::{FromRadioMessage, LedCommand, RadioMetadata, ToRadioMessage};
use alloc::boxed::Box;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Sender;
use embassy_time::Instant;

/// Channel utilization metrics, always updated and read together.
#[derive(Clone, Copy, Default)]
pub struct ChannelMetrics {
    pub channel_util: f32,
    pub air_util_tx: f32,
}

#[derive(Clone)]
pub enum MeshEvent {
    LoraRx(Box<RadioFrame>, RadioMetadata),
    BleRx(Box<ToRadioMessage>),
    BleConnected,
    BleDisconnected,
    BondSave(Box<[u8; 48]>),
    BatteryUpdate(u8, u16),      // level_percent, voltage_mv
    ChannelUtilUpdate(f32, f32), // channel_util_pct, air_util_tx_pct
    Tick,
}

pub struct MeshCtx<'a, S> {
    // Owned mutable state
    pub device: &'a mut DeviceState,
    pub node_db: &'a mut NodeDB,
    pub storage: &'a mut S,
    pub router: &'a mut MeshRouter,
    pub pending_acks: &'a mut heapless::Vec<PendingAck, 8>,
    pub pending_rebroadcast: &'a mut Option<PendingRebroadcast>,
    pub my_position_bytes: &'a mut heapless::Vec<u8, 64>,
    pub session_passkey: &'a mut Option<[u8; 16]>,
    pub from_radio_id: &'a mut u32,
    pub ble_connected: &'a mut bool,
    pub last_nodeinfo_tx: &'a mut Option<Instant>,
    pub last_position_tx: &'a mut Instant,
    pub last_lora_telemetry: &'a mut Option<Instant>,
    pub last_neighborinfo_tx: &'a mut Option<Instant>,
    pub channel_metrics: &'a mut ChannelMetrics,

    // Read-only / Copy
    pub node_id_str: &'a str,
    pub boot_time: Instant,

    // I/O handles (Embassy Sender is Copy — just a &'static Channel ptr)
    pub tx_to_ble: Sender<'static, CriticalSectionRawMutex, FromRadioMessage, 20>,
    pub tx_to_lora: Sender<'static, CriticalSectionRawMutex, RadioFrame, 5>,
    pub led_commands: Sender<'static, CriticalSectionRawMutex, LedCommand, 5>,
}
