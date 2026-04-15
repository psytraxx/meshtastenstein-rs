use crate::{
    constants::{MAX_CHANNEL_UTIL_PCT, POLITE_CHANNEL_UTIL_PCT, POLITE_DUTY_CYCLE_FRACTION},
    domain::{
        device::DeviceState,
        node_db::NodeDB,
        packet::RadioFrame,
        radio_config::Region,
        router::{MeshRouter, PendingPacket, PendingRebroadcast},
    },
    inter_task::channels::{FromRadioMessage, LedCommand},
};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Sender};
use embassy_time::Instant;

/// Channel utilization metrics, always updated and read together.
///
/// Mirrors the two counters upstream exposes on `AirTime`:
/// - `channel_util` — total rolling-window airtime (RX+TX) as a percentage
/// - `air_util_tx`  — rolling-window TX airtime as a percentage
///
/// Both are sampled over a 1-hour window by `lora_task` and refreshed every 30s.
#[derive(Clone, Copy, Default)]
pub struct ChannelMetrics {
    pub channel_util: f32,
    pub air_util_tx: f32,
}

impl ChannelMetrics {
    /// True when the shared medium is quiet enough for a new TX.
    ///
    /// When `polite` is true the caller is background traffic (NodeInfo, Position,
    /// telemetry, NeighborInfo) and uses the tighter `POLITE_CHANNEL_UTIL_PCT`
    /// ceiling; impolite callers (routing ACKs, admin replies, user text) fall
    /// back to the hard `MAX_CHANNEL_UTIL_PCT` ceiling. Matches upstream
    /// `AirTime::isTxAllowedChannelUtil(bool polite)`.
    pub fn is_tx_allowed_channel_util(&self, polite: bool) -> bool {
        let ceiling = if polite {
            POLITE_CHANNEL_UTIL_PCT
        } else {
            MAX_CHANNEL_UTIL_PCT
        };
        self.channel_util < ceiling
    }

    /// True when our own TX airtime is below the region's regulatory ceiling.
    ///
    /// When `polite` is true we only spend a fraction of the regulatory budget
    /// (half by default), leaving headroom for peers sharing the same channel.
    /// Matches upstream `AirTime::isTxAllowedAirUtil()` with region lookup.
    pub fn is_tx_allowed_air_util(&self, region: Region, polite: bool) -> bool {
        let ceiling = region.duty_cycle_pct();
        let effective = if polite {
            ceiling * POLITE_DUTY_CYCLE_FRACTION
        } else {
            ceiling
        };
        self.air_util_tx < effective
    }

    /// Combined gate used by background broadcast builders.
    pub fn tx_allowed_polite(&self, region: Region) -> bool {
        self.is_tx_allowed_channel_util(true) && self.is_tx_allowed_air_util(region, true)
    }

    /// Combined gate used by impolite (but still non-critical) traffic.
    pub fn tx_allowed_impolite(&self, region: Region) -> bool {
        self.is_tx_allowed_channel_util(false) && self.is_tx_allowed_air_util(region, false)
    }
}

pub struct MeshCtx<'a, S> {
    // Owned mutable state
    pub device: &'a mut DeviceState,
    pub node_db: &'a mut NodeDB,
    pub storage: &'a mut S,
    pub router: &'a mut MeshRouter,
    pub pending_packets: &'a mut heapless::Vec<PendingPacket, 8>,
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

    /// Set by admin handlers to request a reboot after N seconds.
    /// The orchestrator checks this after each dispatch and performs the actual reset.
    pub reboot_after_secs: &'a mut Option<u32>,

    /// Set by admin handlers to request a real power-off after N seconds.
    /// The orchestrator forwards this to the watchdog task, which owns the
    /// `DeepSleepAdapter`. Distinct from `reboot_after_secs` — that path resets
    /// the CPU, this one parks the radio in deep sleep until a wake event.
    pub shutdown_after_secs: &'a mut Option<u32>,

    // Read-only / Copy
    pub node_id_str: &'a str,
    pub boot_time: Instant,
    /// Our own X25519 public key. Included in outgoing NodeInfo so peers can
    /// send us PKC direct messages.
    pub pkc_pub_bytes: &'a [u8; 32],
    /// Our own X25519 private key seed. Used to derive shared secrets when
    /// encrypting direct messages via PKC. Never leaves this device.
    pub pkc_priv_bytes: &'a [u8; 32],

    // I/O handles (Embassy Sender is Copy — just a &'static Channel ptr)
    pub tx_to_ble: Sender<'static, CriticalSectionRawMutex, FromRadioMessage, 48>,
    pub tx_to_lora: Sender<'static, CriticalSectionRawMutex, RadioFrame, 5>,
    pub led_commands: Sender<'static, CriticalSectionRawMutex, LedCommand, 5>,
}
