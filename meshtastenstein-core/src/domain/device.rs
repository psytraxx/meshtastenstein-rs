//! Device state: node identity, configuration, role

use crate::{
    constants::DEFAULT_HOP_LIMIT,
    domain::{channels::ChannelSet, handlers::util::hex_byte, radio_config::ModemPreset},
};

/// Meshtastic device role — re-exported from proto to avoid duplication.
pub use crate::proto::config::device_config::Role as DeviceRole;

/// Core device state
pub struct DeviceState {
    /// Our unique node number (derived from MAC)
    pub my_node_num: u32,
    /// MAC address (6 bytes)
    pub mac: [u8; 6],
    /// Short name (4 chars)
    pub short_name: heapless::String<5>,
    /// Long name
    pub long_name: heapless::String<40>,
    /// Hardware model ID
    pub hw_model: u32,
    /// Device role
    pub role: DeviceRole,
    /// Active modem preset (used when use_preset=true)
    pub modem_preset: ModemPreset,
    /// Region code (EU_433 = 2 per LoRaConfig.RegionCode)
    pub region: u8,
    /// If true, use modem_preset; if false, use custom_sf/bw/cr
    pub use_preset: bool,
    /// Custom spreading factor (5–12, valid when use_preset=false). Matches
    /// upstream's `LORA_SF_MIN`/`LORA_SF_MAX` (`src/mesh/MeshRadio.h`); an
    /// out-of-range value from the phone is clamped to 11 (`LORA_SF_DEFAULT`),
    /// same as upstream's `clampSpreadFactor`.
    pub custom_sf: u8,
    /// Custom bandwidth in Hz (valid when use_preset=false). An unmappable
    /// bandwidth code from the phone is clamped to 250 kHz
    /// (`LORA_BW_DEFAULT_KHZ`), same as upstream's `clampBandwidthKHz`.
    pub custom_bw_hz: u32,
    /// Custom coding rate denominator (4–8, valid when use_preset=false).
    /// Matches upstream's `LORA_CR_MIN`/`LORA_CR_MAX`; an out-of-range value
    /// is clamped to 5 (`LORA_CR_DEFAULT`), same as `clampCodingRate`.
    pub custom_cr: u8,
    /// Explicit LoRa channel slot (0 = compute from primary channel hash)
    pub channel_num: u32,
    /// Hop limit applied to originated packets that don't set one
    /// explicitly (`TxBuilder::hop_limit == None`). 0 is a legitimate
    /// "never relayed" setting, matching upstream; clamped to
    /// `MAX_HOP_LIMIT` since the wire field is only 3 bits.
    pub hop_limit: u8,
    /// Whether this node is allowed to transmit on LoRa at all. Takes
    /// effect immediately (no reboot needed) via a shared flag the LoRa
    /// task checks per-frame — see `inter_task::channels::Channels::tx_enabled`.
    pub tx_enabled: bool,
    /// Channel configuration
    pub channels: ChannelSet,
    /// Packet ID counter (monotonically increasing)
    next_packet_id: u32,
}

impl DeviceState {
    /// Create new device state from MAC address
    pub fn new(mac: &[u8; 6]) -> Self {
        // Node number derived from last 4 bytes of MAC (Meshtastic convention)
        let my_node_num = u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]);

        // Generate short name from last 2 MAC bytes
        let mut short_name = heapless::String::new();
        for &b in &mac[4..6] {
            let [hi, lo] = hex_byte(b);
            let _ = short_name.push(hi);
            let _ = short_name.push(lo);
        }

        // Long name
        let mut long_name: heapless::String<40> = heapless::String::new();
        let _ = long_name.push_str("Meshtastic ");
        let _ = long_name.push_str(short_name.as_str());

        Self {
            my_node_num,
            mac: *mac,
            short_name,
            long_name,
            hw_model: 43, // HELTEC_V3
            role: DeviceRole::default(),
            modem_preset: ModemPreset::default(),
            region: 2, // EU_433
            use_preset: true,
            custom_sf: 11,
            custom_bw_hz: 250_000,
            custom_cr: 5,
            channel_num: 0,
            hop_limit: DEFAULT_HOP_LIMIT,
            tx_enabled: true,
            channels: ChannelSet::new(),
            next_packet_id: my_node_num, // Start from node num for uniqueness
        }
    }

    /// Generate next unique packet ID
    pub fn next_packet_id(&mut self) -> u32 {
        self.next_packet_id = self.next_packet_id.wrapping_add(1);
        self.next_packet_id
    }

    /// Derive LoRa modem config and frequency from current device state
    pub fn lora_params(&self) -> (crate::domain::radio_config::ModemConfig, u32) {
        use crate::domain::radio_config::{ModemConfig, Region};

        let region = Region::from_proto(self.region);
        let modem_cfg = if self.use_preset {
            self.modem_preset.config()
        } else {
            ModemConfig {
                spreading_factor: self.custom_sf,
                bandwidth_hz: self.custom_bw_hz,
                coding_rate: self.custom_cr,
            }
        };

        let channel_idx = if self.channel_num != 0 {
            self.channel_num.saturating_sub(1)
        } else {
            region.default_channel_index(self.modem_preset)
        };

        let freq = region.frequency_hz(modem_cfg.bandwidth_hz, channel_idx);
        (modem_cfg, freq)
    }
}
