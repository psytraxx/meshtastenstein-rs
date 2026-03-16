//! NVS Storage Adapter - Persistent radio frame storage and device config using flash memory
//!
//! Flash layout within the NVS partition (each sector = 4096 bytes):
//!   Sector 0 (0x0000–0x0FFF): Device config (SavedConfig, 512 bytes at offset 0)
//!   Sector 1 (0x1000–0x1FFF): BLE bond data (48 bytes at offset 0)
//!   Sector 2+ (0x2000+):      Message ring buffer header + indices
//!
//! Each sector is erased before write (NOR flash: bits can only go 1→0 without erase).

use crate::constants::{MAX_BUFFERED_MESSAGES, MAX_LORA_PAYLOAD_LEN};
use crate::domain::channels::{ChannelConfig, ChannelRole};
use crate::domain::device::{DeviceRole, DeviceState};
use crate::domain::packet::RadioFrame;
use crate::domain::radio_config::ModemPreset;
use crate::ports::{ConfigStorage, Storage as StorageTrait, StorageError};
use embedded_storage::{ReadStorage, Storage};
use esp_bootloader_esp_idf::partitions::{self, DataPartitionSubType, PartitionType};
use esp_storage::FlashStorage;
use log::{error, info, warn};

const STORAGE_OFFSET: u32 = 0x2000;
const HEADER_SIZE: usize = 64;
const MAGIC: u32 = 0x4D455348; // "MESH"

// ── Device config persistence ──────────────────────────────────────────────

/// Offset within NVS partition for device config (sector 0)
const CONFIG_OFFSET: u32 = 0x0000;
const CONFIG_SIZE: usize = 512;
const CONFIG_MAGIC: u32 = 0x4D434647; // "MCFG"
const CONFIG_VERSION: u8 = 2; // v2: separate sectors + custom LoRa params

// Bond storage in its own sector (sector 1) to allow independent erase/write
const BOND_OFFSET: u32 = 0x1000;
pub const BOND_SIZE: usize = 48;
const BOND_MAGIC: u32 = 0x424F4E44; // "BOND"
const BOND_VERSION: u8 = 1;

/// Per-channel data stored in flash (48 bytes each, 8 slots)
#[derive(Clone, Copy, Default)]
struct SavedChannel {
    pub index: u8,
    pub role: u8,    // 0=Disabled, 1=Primary, 2=Secondary
    pub psk_len: u8, // 0, 16, or 32
    pub psk: [u8; 32],
    pub name_len: u8,
    pub name: [u8; 12],
}

/// Device configuration persisted to flash
#[derive(Clone, Copy)]
struct SavedConfig {
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
    pub spread_factor: u8,  // 7–12
    pub bandwidth_khz: u16, // 62, 125, 250, or 500
    pub coding_rate: u8,    // 5–8 (denominator of 4/x)
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

#[derive(Clone, Copy)]
struct CachedSlot {
    valid: bool,
    len: usize,
    data: [u8; MAX_LORA_PAYLOAD_LEN],
}

impl Default for CachedSlot {
    fn default() -> Self {
        Self {
            valid: false,
            len: 0,
            data: [0u8; MAX_LORA_PAYLOAD_LEN],
        }
    }
}

pub struct NvsStorageAdapter<'a> {
    head: usize,
    tail: usize,
    count: usize,
    slots: [CachedSlot; MAX_BUFFERED_MESSAGES],
    flash: FlashStorage<'a>,
    nvs_offset: u32,
    dirty: bool,
}

impl<'a> NvsStorageAdapter<'a> {
    pub fn new(flash_peripheral: esp_hal::peripherals::FLASH<'a>) -> Self {
        info!("[NVS] Initializing flash storage...");
        let mut flash = FlashStorage::new(flash_peripheral);

        let mut pt_mem = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
        let pt = partitions::read_partition_table(&mut flash, &mut pt_mem)
            .expect("Failed to read partition table");

        let nvs_partition = pt
            .find_partition(PartitionType::Data(DataPartitionSubType::Nvs))
            .expect("Failed to find NVS partition")
            .expect("NVS partition not found");

        let nvs_offset = nvs_partition.offset();
        info!("[NVS] NVS at offset 0x{:08X}", nvs_offset);

        let mut adapter = Self {
            head: 0,
            tail: 0,
            count: 0,
            slots: [CachedSlot::default(); MAX_BUFFERED_MESSAGES],
            flash,
            nvs_offset,
            dirty: false,
        };

        adapter.load_or_init();
        adapter
    }

    fn storage_base(&self) -> u32 {
        self.nvs_offset + STORAGE_OFFSET
    }

    fn load_or_init(&mut self) {
        let mut header = [0u8; HEADER_SIZE];
        let base = self.storage_base();

        if self.flash.read(base, &mut header).is_err() {
            self.init_empty();
            return;
        }

        let magic = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        if magic != MAGIC {
            info!("[NVS] Fresh storage, initializing...");
            self.init_empty();
            return;
        }

        self.head = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        self.tail = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
        self.count = u32::from_le_bytes([header[12], header[13], header[14], header[15]]) as usize;

        if self.head >= MAX_BUFFERED_MESSAGES
            || self.tail >= MAX_BUFFERED_MESSAGES
            || self.count > MAX_BUFFERED_MESSAGES
        {
            self.init_empty();
            return;
        }

        info!("[NVS] Restored: {} buffered frames", self.count);
    }

    fn init_empty(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.count = 0;
        self.slots = [CachedSlot::default(); MAX_BUFFERED_MESSAGES];
        self.dirty = true;
        self.persist_header();
    }

    /// Load device config from flash sector 0 of the NVS partition.
    /// Returns `None` if magic is wrong (first boot or corrupted).
    fn load_config(&mut self) -> Option<SavedConfig> {
        let base = self.nvs_offset + CONFIG_OFFSET;
        let mut buf = [0u8; CONFIG_SIZE];
        if self.flash.read(base, &mut buf).is_err() {
            warn!("[NVS] Config read failed");
            return None;
        }
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != CONFIG_MAGIC || buf[4] != CONFIG_VERSION {
            info!("[NVS] No saved config (first boot or version mismatch)");
            return None;
        }

        let long_name_len = buf[5].min(40);
        let mut long_name = [0u8; 40];
        long_name[..long_name_len as usize].copy_from_slice(&buf[6..6 + long_name_len as usize]);

        let short_name_len = buf[46].min(5);
        let mut short_name = [0u8; 5];
        short_name[..short_name_len as usize]
            .copy_from_slice(&buf[47..47 + short_name_len as usize]);

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
        let cfg = SavedConfig {
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
        };

        info!(
            "[NVS] Config loaded: region={} preset={} use_preset={} channels={}",
            cfg.region, cfg.modem_preset, cfg.use_preset, cfg.num_channels
        );
        Some(cfg)
    }

    /// Save device config to flash sector 0 of the NVS partition.
    /// Erases the sector first (NOR flash requirement: cannot set bits 0→1 without erase).
    fn save_config(&mut self, cfg: &SavedConfig) {
        let base = self.nvs_offset + CONFIG_OFFSET;

        // Erase the config sector before writing (4096-byte sector, NOR flash requirement)
        if let Err(e) =
            embedded_storage::nor_flash::NorFlash::erase(&mut self.flash, base, base + 0x1000)
        {
            error!("[NVS] Config erase failed: {:?}", e);
            return;
        }

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
            buf[off + 3..off + 3 + ch.psk_len as usize]
                .copy_from_slice(&ch.psk[..ch.psk_len as usize]);
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

        if let Err(e) = self.flash.write(base, &buf) {
            error!("[NVS] Config write failed: {:?}", e);
        } else {
            info!("[NVS] Config saved");
        }
    }

    /// Load BLE bond from flash. Returns raw 48-byte blob or None if absent/corrupt.
    fn load_bond_internal(&mut self) -> Option<[u8; BOND_SIZE]> {
        let base = self.nvs_offset + BOND_OFFSET;
        let mut buf = [0u8; BOND_SIZE];
        if self.flash.read(base, &mut buf).is_err() {
            return None;
        }
        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != BOND_MAGIC || buf[4] != BOND_VERSION {
            return None;
        }
        info!("[NVS] Bond loaded from flash");
        Some(buf)
    }

    /// Save BLE bond to flash (48-byte raw blob from BLE task).
    /// Erases the bond sector first (NOR flash requirement).
    fn save_bond_internal(&mut self, bytes: &[u8; BOND_SIZE]) {
        let base = self.nvs_offset + BOND_OFFSET;

        // Erase the bond sector before writing (4096-byte sector, NOR flash requirement)
        if let Err(e) =
            embedded_storage::nor_flash::NorFlash::erase(&mut self.flash, base, base + 0x1000)
        {
            error!("[NVS] Bond erase failed: {:?}", e);
            return;
        }

        if let Err(e) = self.flash.write(base, bytes) {
            error!("[NVS] Bond write failed: {:?}", e);
        } else {
            info!("[NVS] Bond saved to flash");
        }
    }

    /// Erase the stored bond (e.g. on pairing failure or explicit clear).
    fn clear_bond_internal(&mut self) {
        let base = self.nvs_offset + BOND_OFFSET;
        let zeroes = [0u8; BOND_SIZE];
        let _ = self.flash.write(base, &zeroes);
        info!("[NVS] Bond cleared");
    }

    fn persist_header(&mut self) {
        let base = self.storage_base();
        let mut header = [0xFFu8; HEADER_SIZE];
        header[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        header[4..8].copy_from_slice(&(self.head as u32).to_le_bytes());
        header[8..12].copy_from_slice(&(self.tail as u32).to_le_bytes());
        header[12..16].copy_from_slice(&(self.count as u32).to_le_bytes());

        if let Err(e) = self.flash.write(base, &header) {
            error!("[NVS] Failed to write header: {:?}", e);
        }
        self.dirty = false;
    }
}

impl<'a> StorageTrait for NvsStorageAdapter<'a> {
    fn add(&mut self, frame: &RadioFrame) -> Result<(), StorageError> {
        if self.count >= MAX_BUFFERED_MESSAGES {
            self.tail = (self.tail + 1) % MAX_BUFFERED_MESSAGES;
            self.count -= 1;
        }

        self.slots[self.head].valid = true;
        self.slots[self.head].len = frame.len;
        self.slots[self.head].data[..frame.len].copy_from_slice(&frame.data[..frame.len]);

        self.head = (self.head + 1) % MAX_BUFFERED_MESSAGES;
        self.count += 1;
        self.dirty = true;
        self.persist_header();
        Ok(())
    }

    fn peek(&mut self) -> Result<Option<RadioFrame>, StorageError> {
        if self.count == 0 {
            return Ok(None);
        }
        let slot = &self.slots[self.tail];
        if !slot.valid {
            return Err(StorageError::StorageError);
        }
        let mut frame = RadioFrame::new();
        frame.data[..slot.len].copy_from_slice(&slot.data[..slot.len]);
        frame.len = slot.len;
        Ok(Some(frame))
    }

    fn pop(&mut self) -> Result<(), StorageError> {
        if self.count == 0 {
            return Err(StorageError::Empty);
        }
        self.slots[self.tail].valid = false;
        self.tail = (self.tail + 1) % MAX_BUFFERED_MESSAGES;
        self.count -= 1;
        self.dirty = true;
        Ok(())
    }

    fn is_empty(&self) -> bool {
        self.count == 0
    }
    fn is_full(&self) -> bool {
        self.count >= MAX_BUFFERED_MESSAGES
    }
    fn count(&self) -> usize {
        self.count
    }

    fn clear(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.count = 0;
        self.slots = [CachedSlot::default(); MAX_BUFFERED_MESSAGES];
        self.dirty = true;
        self.persist_header();
    }
}

impl<'a> ConfigStorage for NvsStorageAdapter<'a> {
    fn save_state(&mut self, device: &DeviceState) {
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
            let psk = ch.effective_psk();
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

        let cfg = SavedConfig {
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
        };

        self.save_config(&cfg);
    }

    fn load_state(&mut self, device: &mut DeviceState) {
        let Some(saved) = self.load_config() else {
            return;
        };

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
        device.role = match saved.role {
            0 => DeviceRole::Client,
            1 => DeviceRole::ClientMute,
            2 => DeviceRole::Router,
            3 => DeviceRole::RouterClient,
            4 => DeviceRole::Repeater,
            5 => DeviceRole::Tracker,
            6 => DeviceRole::Sensor,
            7 => DeviceRole::Tak,
            8 => DeviceRole::ClientHidden,
            9 => DeviceRole::LostAndFound,
            10 => DeviceRole::TakTracker,
            _ => DeviceRole::default(),
        };

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

        info!(
            "[NVS] Config restored: {} ({}) region={}",
            device.long_name.as_str(),
            device.short_name.as_str(),
            device.region
        );
    }

    fn save_bond(&mut self, bytes: &[u8; 48]) {
        self.save_bond_internal(bytes);
    }

    fn load_bond(&mut self) -> Option<[u8; 48]> {
        self.load_bond_internal()
    }

    fn clear_bond(&mut self) {
        self.clear_bond_internal();
    }
}
