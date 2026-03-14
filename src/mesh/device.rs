//! Device state: node identity, configuration, role

use crate::mesh::channels::ChannelSet;
use crate::mesh::radio_config::ModemPreset;

/// Meshtastic device role
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[repr(u8)]
pub enum DeviceRole {
    #[default]
    Client = 0,
    ClientMute = 1,
    Router = 2,
    RouterClient = 3,
    Repeater = 4,
    Tracker = 5,
    Sensor = 6,
    Tak = 7,
    ClientHidden = 8,
    LostAndFound = 9,
    TakTracker = 10,
}

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
    /// Active modem preset
    pub modem_preset: ModemPreset,
    /// Region code (EU_433 = 2 per LoRaConfig.RegionCode)
    pub region: u8,
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
        let hex_chars = b"0123456789abcdef";
        for &b in &mac[4..6] {
            let _ = short_name.push(hex_chars[(b >> 4) as usize] as char);
            let _ = short_name.push(hex_chars[(b & 0x0f) as usize] as char);
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
            channels: ChannelSet::new(),
            next_packet_id: my_node_num, // Start from node num for uniqueness
        }
    }

    /// Generate next unique packet ID
    pub fn next_packet_id(&mut self) -> u32 {
        self.next_packet_id = self.next_packet_id.wrapping_add(1);
        self.next_packet_id
    }
}
