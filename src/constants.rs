//! Meshtastic firmware configuration constants for Heltec WiFi LoRa V3

//==============================================================================
// Meshtastic LoRa Radio Configuration
//==============================================================================

/// Meshtastic LoRa sync word (0x2B for SX126x, corresponds to 0x12 for SX127x)
/// This must be set via SX1262 register 0x0740/0x0741
pub const MESHTASTIC_SYNC_WORD: u16 = 0x2B;

/// SX1262 sync word register MSB: value = (sync_word & 0xF0) | 0x04 = 0x24
pub const SX1262_SYNC_WORD_MSB: u8 = 0x24;
/// SX1262 sync word register LSB: value = ((sync_word & 0x0F) << 4) | 0x04 = 0xB4
pub const SX1262_SYNC_WORD_LSB: u8 = 0xB4;

/// Meshtastic preamble length (16 symbols for all presets)
pub const MESHTASTIC_PREAMBLE_LENGTH: u16 = 16;

/// Maximum LoRa payload size for Meshtastic
pub const MAX_LORA_PAYLOAD_LEN: usize = 255;

/// Maximum Meshtastic mesh packet payload (after 16-byte header)
pub const MAX_MESH_PAYLOAD_LEN: usize = 239;

/// LoRa TX power in dBm
pub const LORA_TX_POWER_DBM: i32 = 22;

/// Default channel PSK (AQ== base64, single byte 0x01 = default "AQ==" key)
/// The actual default key used when PSK is [0x01] is the well-known Meshtastic default:
pub const DEFAULT_PSK: [u8; 16] = [
    0xd4, 0xf1, 0xbb, 0x3a, 0x20, 0x29, 0x07, 0x59, 0xf0, 0xbc, 0xff, 0xab, 0xcf, 0x4e, 0x69, 0x01,
];

/// Default hop limit for new packets
pub const DEFAULT_HOP_LIMIT: u8 = 3;

/// Maximum hop limit
pub const MAX_HOP_LIMIT: u8 = 7;

//==============================================================================
// Meshtastic BLE Configuration
//==============================================================================
// UUIDs are defined as string literals in ble_task.rs (required by #[gatt_service] macros):
//   Service:   6ba1b218-15a8-461f-9fa8-5dcae273eafd
//   ToRadio:   f75c76d2-129e-4dad-a1dd-7866124401e7
//   FromRadio: 2c55e69e-4993-11ed-b878-0242ac120002
//   FromNum:   ed9da18c-a800-4f66-a670-aa7547e34453

/// BLE device name prefix
pub const BLE_DEVICE_NAME_PREFIX: &str = "Meshtastic_";

/// BLE advertising interval min (ms)
pub const BLE_ADV_INTERVAL_MIN_MS: u64 = 100;
/// BLE advertising interval max (ms)
pub const BLE_ADV_INTERVAL_MAX_MS: u64 = 300;

//==============================================================================
// EU 433 MHz Frequency Configuration
//==============================================================================

/// Default channel index for LongFast preset in EU_433 region (hash-based, channel_num=0).
/// hash = XOR("LongFast") = 0x0A = 10; num_channels = 4; index = 10 % 4 = 2
pub const DEFAULT_CHANNEL_INDEX: u32 = 2;

/// Default frequency for LongFast preset, EU_433 region, channel index 2:
/// 433.000 + 250kHz/2 + 2 × 250kHz = 433.625 MHz
pub const DEFAULT_FREQUENCY_HZ: u32 = 433_625_000;

//==============================================================================
// GPIO Pin Configuration (Heltec WiFi LoRa V3)
//==============================================================================

pub mod heltec_wifi_lora_v3 {
    /// LoRa SPI SCK pin
    pub const LORA_SCK: u8 = 9;
    /// LoRa SPI MISO pin
    pub const LORA_MISO: u8 = 11;
    /// LoRa SPI MOSI pin
    pub const LORA_MOSI: u8 = 10;
    /// LoRa SPI CS (chip select) pin
    pub const LORA_SS: u8 = 8;
    /// LoRa reset pin
    pub const LORA_RST: u8 = 12;
    /// LoRa DIO1 interrupt pin
    pub const LORA_DIO1: u8 = 14;
    /// LoRa BUSY pin
    pub const LORA_BUSY: u8 = 13;
    /// LED pin (active HIGH)
    pub const LED_PIN: u8 = 35;
    /// Wake button pin (active LOW with pull-up)
    pub const WAKE_BUTTON: u8 = 0;
    /// VEXT control pin
    pub const VEXT_PIN: u8 = 36;
    /// Battery voltage ADC pin
    pub const BATTERY_ADC_PIN: u8 = 1;
    /// Battery ADC control pin
    pub const BATTERY_ADC_CTRL: u8 = 37;
    /// Battery voltage divider ratio
    pub const BATTERY_VOLTAGE_DIVIDER: f32 = 5.1205;
}

//==============================================================================
// Power Management Configuration
//==============================================================================

/// Inactivity timeout before deep sleep (ms)
pub const INACTIVITY_TIMEOUT_MS: u64 = 300_000; // 5 minutes for mesh router

/// Watchdog timeout (seconds)
pub const WATCHDOG_TIMEOUT_SECS: u64 = 10;

//==============================================================================
// LED Configuration
//==============================================================================

pub const LED_ON_MS: u64 = 50;
pub const LED_BLINK_DELAY_MS: u64 = 200;
pub const LED_HEARTBEAT_INTERVAL_MS: u64 = 2000;
pub const LED_HEARTBEAT_ON_MS: u64 = 5;

//==============================================================================
// CAD Configuration
//==============================================================================

pub const CAD_MAX_RETRIES: u8 = 5;
pub const CAD_BACKOFF_BASE_MS: u64 = 50;
pub const CAD_BACKOFF_JITTER_MS: u64 = 100;

//==============================================================================
// Mesh Configuration
//==============================================================================

/// NodeInfo broadcast interval (15 minutes, Meshtastic default)
pub const NODEINFO_BROADCAST_INTERVAL_MS: u64 = 900_000;

/// Delay after boot before sending the first NodeInfo broadcast
pub const NODEINFO_BOOT_DELAY_MS: u64 = 5_000;

/// want_ack retransmit timeout (ms) — M1
pub const WANT_ACK_TIMEOUT_MS: u64 = 5_000;

/// Maximum want_ack retransmit attempts — M1
pub const WANT_ACK_MAX_RETRIES: u8 = 3;

/// Position re-broadcast interval (30 minutes) — M6
pub const POSITION_BROADCAST_INTERVAL_MS: u64 = 1_800_000;

/// Duplicate detection ring buffer size
pub const DUPLICATE_RING_SIZE: usize = 64;

/// NodeDB maximum entries
pub const MAX_NODES: usize = 64;

/// Maximum channels
pub const MAX_CHANNELS: usize = 8;

/// Maximum buffered messages for NVS storage
pub const MAX_BUFFERED_MESSAGES: usize = 10;

//==============================================================================
// Battery Monitoring
//==============================================================================

pub const OCV_TABLE: [u16; 11] = [
    4200, 4050, 3900, 3800, 3730, 3680, 3630, 3570, 3500, 3400, 3100,
];
