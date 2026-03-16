//! Inter-task communication channels for Meshtastic firmware
//!
//! Channel topology:
//! ```text
//!                     ┌──────────────┐
//!                     │   Watchdog   │
//!                     └──────┬───────┘
//!                            │ disconn_cmd
//!                            ▼
//! ┌─────────┐  ble_rx   ┌─────────────┐  lora_tx   ┌─────────┐
//! │   BLE   │◄─────────►│  Mesh Task  │◄──────────►│  LoRa   │
//! │  Task   │  ble_tx   │             │  lora_rx   │  Task   │
//! └────┬────┘           └──────┬──────┘            └─────────┘
//!      │ conn_state            │ led_cmd
//!      │                       ▼
//!      │               ┌─────────────┐
//!      │               │  LED Task   │
//!      │               └─────────────┘
//!      │ bat_level
//!      ▼
//! ┌─────────┐
//! │ Battery │
//! │  Task   │
//! └─────────┘
//! ```

use crate::domain::packet::RadioFrame;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal; // also used for bat_level (broadcast semantics)
use embassy_time::Instant;

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
    pub data: heapless::Vec<u8, 512>,
    /// The `id` field of the enclosed `FromRadio` message.
    /// BLE task writes this to the `FromNum` characteristic so the phone knows
    /// the exact packet ID that just arrived (N4 fix).
    pub id: u32,
}

/// Wrapper for ToRadio messages received from BLE
#[derive(Clone)]
pub struct ToRadioMessage {
    pub data: heapless::Vec<u8, 512>,
}

/// All inter-task communication channels
pub struct Channels {
    /// LoRa → Mesh: Received radio frames with metadata (capacity: 5)
    pub lora_rx: Channel<CriticalSectionRawMutex, (RadioFrame, RadioMetadata), 5>,

    /// Mesh → LoRa: Radio frames to transmit (capacity: 5)
    pub lora_tx: Channel<CriticalSectionRawMutex, RadioFrame, 5>,

    /// BLE → Mesh: ToRadio messages from phone (capacity: 5)
    pub ble_rx: Channel<CriticalSectionRawMutex, ToRadioMessage, 5>,

    /// Mesh → BLE: FromRadio messages to phone (capacity: 20)
    pub ble_tx: Channel<CriticalSectionRawMutex, FromRadioMessage, 20>,

    /// Mesh → LED: Blink pattern commands (capacity: 5)
    pub led_cmd: Channel<CriticalSectionRawMutex, LedCommand, 5>,

    /// Battery → Mesh: Battery level percentage (Signal = last-writer-wins, mesh task observes)
    /// Battery: (level_percent 0-100, voltage_mv)
    pub bat_level: Signal<CriticalSectionRawMutex, (u8, u16)>,

    /// BLE → Mesh: Connection state changes (capacity: 1)
    pub conn_state: Channel<CriticalSectionRawMutex, bool, 1>,

    /// Watchdog → BLE: Disconnect command on inactivity timeout (capacity: 1)
    pub disconn_cmd: Channel<CriticalSectionRawMutex, (), 1>,

    /// Mesh → Watchdog: Activity signal (instant delivery)
    pub activity: Signal<CriticalSectionRawMutex, Instant>,

    /// LoRa → BLE: Last received signal quality (RSSI dBm, SNR dB)
    pub radio_stats: Signal<CriticalSectionRawMutex, (i16, i8)>,

    /// BLE → Mesh: Serialized bond bytes to persist in NVS (capacity: 1)
    pub bond_save: Channel<CriticalSectionRawMutex, [u8; 48], 1>,

    /// LoRa → Mesh: Channel utilization (channel_util_pct, air_util_tx_pct)
    pub channel_util: Signal<CriticalSectionRawMutex, (f32, f32)>,
}

impl Channels {
    pub const fn new() -> Self {
        Self {
            lora_rx: Channel::new(),
            lora_tx: Channel::new(),
            ble_rx: Channel::new(),
            ble_tx: Channel::new(),
            led_cmd: Channel::new(),
            bat_level: Signal::new(),
            conn_state: Channel::new(),
            disconn_cmd: Channel::new(),
            activity: Signal::new(),
            radio_stats: Signal::new(),
            bond_save: Channel::new(),
            channel_util: Signal::new(),
        }
    }
}

impl Default for Channels {
    fn default() -> Self {
        Self::new()
    }
}
