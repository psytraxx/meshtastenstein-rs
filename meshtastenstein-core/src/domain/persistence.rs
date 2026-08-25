//! Board-agnostic NVS record layouts: encode/decode only, no flash I/O.
//!
//! Every board's storage adapter needs to serialize the same handful of
//! records (device config, BLE bond, PKC keypair, buffered-message slots) to
//! flash-storable byte blobs. The record layouts — magic numbers, versions,
//! field offsets — are identical across boards; only how the bytes reach
//! flash differs. The ESP32 adapter is built on the synchronous
//! `embedded_storage` traits; the nRF52 board's `mpsl::Flash` only
//! implements the async `embedded_storage_async::nor_flash::NorFlash` for
//! writes (MPSL arbitrates flash access against radio activity, so it can't
//! block), so a shared module can't perform I/O itself the way
//! `drivers::lora_task_body` does for the radio.
//!
//! What *is* shared: everything here takes and returns fixed-size byte
//! arrays, with zero flash calls. Each board's adapter is a thin wrapper —
//! sync or async — around `flash.read()`/`.write()`/`.erase()` plus a call
//! into one of these functions. Keeping the layout logic here once means a
//! future field never needs editing in two places.

use crate::domain::{
    channels::{ChannelConfig, ChannelRole},
    device::{DeviceRole, DeviceState},
    radio_config::ModemPreset,
};

// ── BLE bond ────────────────────────────────────────────────────────────────

pub const BOND_SIZE: usize = 48;
const BOND_MAGIC: u32 = 0x424F4E44; // "BOND"
const BOND_VERSION: u8 = 2;

/// Recognize a bond blob's magic/version without decoding it — used by a
/// board's own bond format (e.g. `trouble_host::BondInformation`, which this
/// module intentionally doesn't depend on) to validate before parsing further.
pub fn bond_magic_valid(b: &[u8; BOND_SIZE]) -> bool {
    let magic = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    magic == BOND_MAGIC && b[4] == BOND_VERSION
}

/// Write the bond blob's magic/version header. Callers fill the remaining
/// bytes with their own BLE-stack-specific bond fields (address, IRK, LTK,
/// security level) — that part isn't board-agnostic since it depends on
/// which trouble-host major the board is pinned to.
pub fn init_bond_header(b: &mut [u8; BOND_SIZE]) {
    b[0..4].copy_from_slice(&BOND_MAGIC.to_le_bytes());
    b[4] = BOND_VERSION;
}

// ── X25519 PKC keypair ──────────────────────────────────────────────────────

pub const PKC_BLOB_SIZE: usize = 4 + 1 + 3 + 32 + 32; // magic + ver + reserved + priv + pub = 72
const PKC_MAGIC: u32 = 0x504B4331; // "PKC1"
const PKC_VERSION: u8 = 1;

/// Encode an X25519 keypair into its 72-byte flash blob.
pub fn encode_pkc_keypair(priv_key: &[u8; 32], pub_key: &[u8; 32]) -> [u8; PKC_BLOB_SIZE] {
    let mut buf = [0u8; PKC_BLOB_SIZE];
    buf[0..4].copy_from_slice(&PKC_MAGIC.to_le_bytes());
    buf[4] = PKC_VERSION;
    // buf[5..8] reserved, left as 0
    buf[8..40].copy_from_slice(priv_key);
    buf[40..72].copy_from_slice(pub_key);
    buf
}

/// Decode a 72-byte PKC blob. Returns `None` on bad magic/version, or an
/// all-zero private key (meaning "not yet generated" — can't be a real key).
pub fn decode_pkc_keypair(buf: &[u8; PKC_BLOB_SIZE]) -> Option<([u8; 32], [u8; 32])> {
    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != PKC_MAGIC || buf[4] != PKC_VERSION {
        return None;
    }
    let mut priv_key = [0u8; 32];
    let mut pub_key = [0u8; 32];
    priv_key.copy_from_slice(&buf[8..40]);
    pub_key.copy_from_slice(&buf[40..72]);
    if priv_key.iter().all(|&b| b == 0) {
        return None;
    }
    Some((priv_key, pub_key))
}

// ── Device config ────────────────────────────────────────────────────────────

pub const CONFIG_SIZE: usize = 512;
const CONFIG_MAGIC: u32 = 0x4D434647; // "MCFG"
const CONFIG_VERSION: u8 = 2;

/// Per-channel data stored in flash (48 bytes each, 8 slots)
#[derive(Clone, Copy, Default)]
pub struct SavedChannel {
    pub index: u8,
    pub role: u8,    // 0=Disabled, 1=Primary, 2=Secondary
    pub psk_len: u8, // 0, 1, 16, or 32
    pub psk: [u8; 32],
    pub name_len: u8,
    pub name: [u8; 12],
}

/// Device configuration persisted to flash
#[derive(Clone, Copy)]
pub struct SavedConfig {
    pub long_name_len: u8,
    pub long_name: [u8; 40],
    pub short_name_len: u8,
    pub short_name: [u8; 5],
    pub region: u8,
    pub modem_preset: u8,
    pub role: u8,
    pub num_channels: u8,
    pub channels: [SavedChannel; 8],
    // Custom LoRa params (used when use_preset == 0)
    pub use_preset: u8,     // 1 = use modem_preset, 0 = use custom params below
    pub spread_factor: u8,  // 7-12
    pub bandwidth_khz: u16, // 62, 125, 250, or 500
    pub coding_rate: u8,    // 5-8 (denominator of 4/x)
    // Explicit channel slot (buf[445..447]); 0 = compute from hash, 0xFFFF = uninitialized
    pub channel_num: u16, // 0 = hash-based (default); >0 = use directly as channel index
}

impl Default for SavedConfig {
    fn default() -> Self {
        Self {
            long_name_len: 0,
            long_name: [0u8; 40],
            short_name_len: 0,
            short_name: [0u8; 5],
            region: 2, // EU_433
            modem_preset: 0,
            role: 0,
            num_channels: 0,
            channels: [SavedChannel::default(); 8],
            use_preset: 1,
            spread_factor: 11,
            bandwidth_khz: 250,
            coding_rate: 5,
            channel_num: 0,
        }
    }
}

/// Build a [`SavedConfig`] from the live [`DeviceState`], ready to encode.
pub fn saved_config_from_device(device: &DeviceState) -> SavedConfig {
    let ln = device.long_name.as_bytes();
    let long_name_len = ln.len() as u8;
    let mut long_name = [0u8; 40];
    long_name[..ln.len()].copy_from_slice(ln);

    let sn = device.short_name.as_bytes();
    let short_name_len = sn.len() as u8;
    let mut short_name = [0u8; 5];
    short_name[..sn.len()].copy_from_slice(sn);

    let mut channels = [SavedChannel::default(); 8];
    let mut num_channels = 0u8;
    for ch in device.channels.active_channels() {
        if num_channels >= 8 {
            break;
        }
        let psk = ch.psk.as_slice();
        let mut psk_arr = [0u8; 32];
        psk_arr[..psk.len()].copy_from_slice(psk);
        let name = ch.name.as_bytes();
        let mut name_arr = [0u8; 12];
        name_arr[..name.len()].copy_from_slice(name);
        channels[num_channels as usize] = SavedChannel {
            index: ch.index,
            role: ch.role as u8,
            psk_len: psk.len() as u8,
            psk: psk_arr,
            name_len: name.len() as u8,
            name: name_arr,
        };
        num_channels += 1;
    }

    SavedConfig {
        long_name_len,
        long_name,
        short_name_len,
        short_name,
        region: device.region,
        modem_preset: device.modem_preset as u8,
        role: device.role as u8,
        use_preset: device.use_preset as u8,
        spread_factor: device.custom_sf,
        bandwidth_khz: (device.custom_bw_hz / 1000) as u16,
        coding_rate: device.custom_cr,
        channel_num: device.channel_num as u16,
        num_channels,
        channels,
    }
}

/// Encode a [`SavedConfig`] into its 512-byte flash blob.
pub fn encode_config(cfg: &SavedConfig) -> [u8; CONFIG_SIZE] {
    let mut buf = [0xFFu8; CONFIG_SIZE];

    buf[0..4].copy_from_slice(&CONFIG_MAGIC.to_le_bytes());
    buf[4] = CONFIG_VERSION;
    buf[5] = cfg.long_name_len;
    buf[6..6 + cfg.long_name_len as usize]
        .copy_from_slice(&cfg.long_name[..cfg.long_name_len as usize]);
    buf[46] = cfg.short_name_len;
    buf[47..47 + cfg.short_name_len as usize]
        .copy_from_slice(&cfg.short_name[..cfg.short_name_len as usize]);
    buf[52] = cfg.region;
    buf[53] = cfg.modem_preset;
    buf[54] = cfg.role;
    buf[55] = cfg.num_channels;

    let ch_base = 56usize;
    for i in 0..cfg.num_channels as usize {
        let off = ch_base + i * 48;
        let ch = &cfg.channels[i];
        buf[off] = ch.index;
        buf[off + 1] = ch.role;
        buf[off + 2] = ch.psk_len;
        buf[off + 3..off + 3 + ch.psk_len as usize].copy_from_slice(&ch.psk[..ch.psk_len as usize]);
        buf[off + 35] = ch.name_len;
        buf[off + 36..off + 36 + ch.name_len as usize]
            .copy_from_slice(&ch.name[..ch.name_len as usize]);
    }

    // Custom LoRa params at buf[440..445]
    buf[440] = cfg.use_preset;
    buf[441] = cfg.spread_factor;
    buf[442..444].copy_from_slice(&cfg.bandwidth_khz.to_le_bytes());
    buf[444] = cfg.coding_rate;
    // channel_num at buf[445..447]
    buf[445..447].copy_from_slice(&cfg.channel_num.to_le_bytes());

    buf
}

/// Decode a 512-byte config blob. Returns `None` on bad magic/version (first
/// boot, or a version this build doesn't understand).
pub fn decode_config(buf: &[u8; CONFIG_SIZE]) -> Option<SavedConfig> {
    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != CONFIG_MAGIC || buf[4] != CONFIG_VERSION {
        return None;
    }

    let long_name_len = buf[5].min(40);
    let mut long_name = [0u8; 40];
    long_name[..long_name_len as usize].copy_from_slice(&buf[6..6 + long_name_len as usize]);

    let short_name_len = buf[46].min(5);
    let mut short_name = [0u8; 5];
    short_name[..short_name_len as usize].copy_from_slice(&buf[47..47 + short_name_len as usize]);

    let num_channels = buf[55].min(8);
    let mut channels = [SavedChannel::default(); 8];
    let ch_base = 56usize;
    for (i, slot) in channels.iter_mut().enumerate().take(num_channels as usize) {
        let off = ch_base + i * 48;
        let psk_len = buf[off + 2].min(32);
        let mut psk = [0u8; 32];
        psk[..psk_len as usize].copy_from_slice(&buf[off + 3..off + 3 + psk_len as usize]);
        let name_len = buf[off + 35].min(12);
        let mut name = [0u8; 12];
        name[..name_len as usize].copy_from_slice(&buf[off + 36..off + 36 + name_len as usize]);
        *slot = SavedChannel {
            index: buf[off],
            role: buf[off + 1],
            psk_len,
            psk,
            name_len,
            name,
        };
    }

    // Custom LoRa params at buf[440..445]
    // channel_num at buf[445..447]; 0xFFFF = uninitialized flash → treat as 0 (hash-based)
    let raw_ch = u16::from_le_bytes([buf[445], buf[446]]);

    Some(SavedConfig {
        long_name_len,
        long_name,
        short_name_len,
        short_name,
        region: buf[52],
        modem_preset: buf[53],
        role: buf[54],
        num_channels,
        channels,
        use_preset: buf[440],
        spread_factor: buf[441],
        bandwidth_khz: u16::from_le_bytes([buf[442], buf[443]]),
        coding_rate: buf[444],
        channel_num: if raw_ch == 0xFFFF { 0 } else { raw_ch },
    })
}

/// Apply a decoded [`SavedConfig`] onto a live [`DeviceState`].
pub fn apply_saved_config(saved: &SavedConfig, device: &mut DeviceState) {
    if saved.long_name_len > 0
        && let Ok(s) = core::str::from_utf8(&saved.long_name[..saved.long_name_len as usize])
    {
        device.long_name = heapless::String::new();
        let _ = device.long_name.push_str(s);
    }
    if saved.short_name_len > 0
        && let Ok(s) = core::str::from_utf8(&saved.short_name[..saved.short_name_len as usize])
    {
        device.short_name = heapless::String::new();
        let _ = device.short_name.push_str(s);
    }

    device.region = saved.region;
    device.modem_preset = ModemPreset::from_proto(saved.modem_preset);
    device.use_preset = saved.use_preset != 0;
    device.custom_sf = saved.spread_factor;
    device.custom_bw_hz = saved.bandwidth_khz as u32 * 1000;
    device.custom_cr = saved.coding_rate;
    device.channel_num = saved.channel_num as u32;
    device.role = DeviceRole::try_from(saved.role as i32).unwrap_or_default();

    for i in 0..saved.num_channels as usize {
        let sc = &saved.channels[i];
        let role = match sc.role {
            1 => ChannelRole::Primary,
            2 => ChannelRole::Secondary,
            _ => continue,
        };
        let mut psk: heapless::Vec<u8, 32> = heapless::Vec::new();
        psk.extend_from_slice(&sc.psk[..sc.psk_len as usize]).ok();
        let mut name: heapless::String<12> = heapless::String::new();
        if let Ok(s) = core::str::from_utf8(&sc.name[..sc.name_len as usize]) {
            let _ = name.push_str(s);
        }
        device.channels.set(
            sc.index,
            ChannelConfig {
                index: sc.index,
                name,
                psk,
                role,
            },
        );
    }
}

// ── Message ring buffer (buffered outbound frames) ─────────────────────────

/// Ring header: magic (4) + head (4) + tail (4) + count (4) = 16 bytes,
/// padded to 64 to leave headroom for future fields without a version bump.
pub const RING_HEADER_SIZE: usize = 64;
const RING_MAGIC: u32 = 0x4D455348; // "MESH"

/// Per-slot on-flash size: 1 (valid flag) + 1 (len) + up to `MAX_LORA_PAYLOAD_LEN`
/// data + padding. Callers pick the total slot size to match their payload
/// cap; this module only handles the 2-byte flag/length prefix.
pub const RING_SLOT_PREFIX_SIZE: usize = 2;

pub struct RingHeader {
    pub head: usize,
    pub tail: usize,
    pub count: usize,
}

/// Encode the ring buffer header.
pub fn encode_ring_header(h: &RingHeader) -> [u8; RING_HEADER_SIZE] {
    let mut buf = [0xFFu8; RING_HEADER_SIZE];
    buf[0..4].copy_from_slice(&RING_MAGIC.to_le_bytes());
    buf[4..8].copy_from_slice(&(h.head as u32).to_le_bytes());
    buf[8..12].copy_from_slice(&(h.tail as u32).to_le_bytes());
    buf[12..16].copy_from_slice(&(h.count as u32).to_le_bytes());
    buf
}

/// Decode a ring buffer header. Returns `None` on bad magic or an
/// out-of-range head/tail/count for the given capacity (corrupt or first boot).
pub fn decode_ring_header(buf: &[u8; RING_HEADER_SIZE], capacity: usize) -> Option<RingHeader> {
    let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if magic != RING_MAGIC {
        return None;
    }
    let head = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    let tail = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]) as usize;
    let count = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]) as usize;
    if head >= capacity || tail >= capacity || count > capacity {
        return None;
    }
    Some(RingHeader { head, tail, count })
}

/// Encode one ring slot's valid flag + length prefix into the first two bytes
/// of `buf`; the caller has already placed the payload data at `buf[2..]`.
pub fn encode_ring_slot_prefix(buf: &mut [u8], valid: bool, len: usize) {
    buf[0] = if valid { 1 } else { 0 };
    buf[1] = len as u8;
}

/// Decode a ring slot's valid flag + length prefix. `0xFF` in the valid byte
/// (erased/uninitialized flash) must not be treated as valid — only exactly
/// `1` is.
pub fn decode_ring_slot_prefix(buf: &[u8]) -> (bool, usize) {
    (buf[0] == 1, buf[1] as usize)
}
