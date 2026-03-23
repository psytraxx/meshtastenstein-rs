//! Inter-task communication channels for Meshtastic firmware
//!
//! All external events funnel into a single typed `mesh_in` channel as `MeshEvent` variants.
//! The mesh orchestrator is the sole consumer; producers (lora_task, ble_task, battery_task)
//! publish directly without knowing about each other.
//!
//! ```text
//! lora_task ──MeshEvent::LoraRx──────────┐
//! lora_task ──MeshEvent::ChannelUtil─────┤
//! ble_task  ──MeshEvent::BleRx───────────┤
//! ble_task  ──MeshEvent::BleConnected────┼──► mesh_in ──► MeshOrchestrator
//! ble_task  ──MeshEvent::BondSave────────┤
//! battery   ──MeshEvent::BatteryUpdate───┘
//!
//! MeshOrchestrator ──► lora_tx    ──► lora_task  (frames to transmit)
//! MeshOrchestrator ──► ble_tx     ──► ble_task   (FromRadio to phone)
//! MeshOrchestrator ──► led_cmd    ──► led_task
//! MeshOrchestrator ──► activity   ──► watchdog_task
//! MeshOrchestrator ──► radio_stats──► ble_task
//!
//! battery_task ──► bat_level ──► ble_task (BLE battery characteristic)
//!                            └─► watchdog_task (low-battery detection)
//! watchdog_task──► disconn_cmd──► ble_task
//! ```

extern crate alloc;
use crate::domain::packet::RadioFrame;
use alloc::boxed::Box;
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel, signal::Signal,
};
use embassy_time::Instant;
use heapless::Vec;

/// RSSI/SNR metadata for a received LoRa packet
#[derive(Debug, Clone, Copy)]
pub struct RadioMetadata {
    pub rssi: i16,
    pub snr: i8,
}

/// LED blink patterns
#[derive(Debug, Clone, Copy)]
pub enum LedPattern {
    SingleBlink,
    DoubleBlink,
    Heartbeat,
}

/// Commands sent to the LED task
#[derive(Debug, Clone, Copy)]
pub enum LedCommand {
    Blink(LedPattern),
}

/// Wrapper for FromRadio messages queued for BLE transmission
#[derive(Clone)]
pub struct FromRadioMessage {
    pub data: Vec<u8, 512>,
    /// The `id` field of the enclosed `FromRadio` message.
    /// BLE task writes this to the `FromNum` characteristic so the phone knows
    /// the exact packet ID that just arrived (N4 fix).
    pub id: u32,
}

/// All events that flow into the mesh orchestrator.
///
/// Producers (lora_task, ble_task, battery_task) push variants directly to `Channels::mesh_in`.
/// Heavy payloads are heap-allocated via `Box` to keep the enum small on the stack
/// (`#![deny(clippy::large_stack_frames)]`).
#[derive(Clone)]
pub enum MeshEvent {
    LoraRx(Box<RadioFrame>, RadioMetadata),
    BleRx(Box<Vec<u8, 512>>),
    BleConnected,
    BleDisconnected,
    BondSave(Box<[u8; 48]>),
    BatteryUpdate(u8, u16),      // level_percent, voltage_mv
    ChannelUtilUpdate(f32, f32), // channel_util_pct, air_util_tx_pct
    Tick,
}

/// All inter-task communication channels
pub struct Channels {
    /// All external events → Mesh orchestrator (capacity: 8)
    pub mesh_in: Channel<CriticalSectionRawMutex, MeshEvent, 8>,

    /// Mesh → LoRa: Radio frames to transmit (capacity: 5)
    pub lora_tx: Channel<CriticalSectionRawMutex, RadioFrame, 5>,

    /// Mesh → BLE: FromRadio messages to phone (capacity: 48)
    /// Must be >= config exchange packet count (~35) to avoid dropping live packets
    /// that arrive before the phone finishes reading the exchange.
    pub ble_tx: Channel<CriticalSectionRawMutex, FromRadioMessage, 48>,

    /// Mesh → LED: Blink pattern commands (capacity: 5)
    pub led_cmd: Channel<CriticalSectionRawMutex, LedCommand, 5>,

    /// Battery → BLE + Watchdog: Battery level (Signal = last-writer-wins)
    /// Mesh task receives battery updates via mesh_in instead.
    pub bat_level: Signal<CriticalSectionRawMutex, (u8, u16)>,

    /// Watchdog → BLE: Disconnect command on inactivity timeout (capacity: 1)
    pub disconn_cmd: Channel<CriticalSectionRawMutex, (), 1>,

    /// Mesh → Watchdog: Activity signal (instant delivery)
    pub activity: Signal<CriticalSectionRawMutex, Instant>,

    /// Mesh → BLE: Last received signal quality (RSSI dBm, SNR dB)
    pub radio_stats: Signal<CriticalSectionRawMutex, (i16, i8)>,
}

impl Channels {
    pub const fn new() -> Self {
        Self {
            mesh_in: Channel::new(),
            lora_tx: Channel::new(),
            ble_tx: Channel::new(),
            led_cmd: Channel::new(),
            bat_level: Signal::new(),
            disconn_cmd: Channel::new(),
            activity: Signal::new(),
            radio_stats: Signal::new(),
        }
    }
}

impl Default for Channels {
    fn default() -> Self {
        Self::new()
    }
}
