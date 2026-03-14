//! NVS Storage Adapter - Persistent radio frame storage and device config using flash memory
//!
//! Flash layout within the NVS partition:
//!   Sector 0 (0x0000–0x0FFF): Device config (SavedConfig, 512 bytes at offset 0)
//!   Sector 1+ (0x1000+):      Message ring buffer (STORAGE_OFFSET)

use crate::constants::{MAX_BUFFERED_MESSAGES, MAX_LORA_PAYLOAD_LEN};
use crate::mesh::packet::RadioFrame;
use crate::ports::{Storage as StorageTrait, StorageError};
use embedded_storage::{ReadStorage, Storage};
use esp_bootloader_esp_idf::partitions::{self, DataPartitionSubType, PartitionType};
use esp_storage::FlashStorage;
use log::{error, info, warn};

const STORAGE_OFFSET: u32 = 0x1000;
const HEADER_SIZE: usize = 64;
const MAGIC: u32 = 0x4D455348; // "MESH"

// ── Device config persistence ──────────────────────────────────────────────

/// Offset within NVS partition for device config (sector 0)
const CONFIG_OFFSET: u32 = 0x0000;
const CONFIG_SIZE: usize = 512;
const CONFIG_MAGIC: u32 = 0x4D434647; // "MCFG"
const CONFIG_VERSION: u8 = 1;

/// Per-channel data stored in flash (48 bytes each, 8 slots)
#[derive(Clone, Copy, Default)]
pub struct SavedChannel {
    pub index: u8,
    pub role: u8,    // 0=Disabled, 1=Primary, 2=Secondary
    pub psk_len: u8, // 0, 16, or 32
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
    #[allow(clippy::field_reassign_with_default)]
    pub fn load_config(&mut self) -> Option<SavedConfig> {
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

        let mut cfg = SavedConfig::default();
        cfg.long_name_len = buf[5].min(40);
        cfg.long_name[..cfg.long_name_len as usize]
            .copy_from_slice(&buf[6..6 + cfg.long_name_len as usize]);
        cfg.short_name_len = buf[46].min(5);
        cfg.short_name[..cfg.short_name_len as usize]
            .copy_from_slice(&buf[47..47 + cfg.short_name_len as usize]);
        cfg.region = buf[52];
        cfg.modem_preset = buf[53];
        cfg.role = buf[54];
        cfg.num_channels = buf[55].min(8);

        let ch_base = 56usize;
        for i in 0..cfg.num_channels as usize {
            let off = ch_base + i * 48;
            cfg.channels[i].index = buf[off];
            cfg.channels[i].role = buf[off + 1];
            cfg.channels[i].psk_len = buf[off + 2].min(32);
            cfg.channels[i].psk[..cfg.channels[i].psk_len as usize]
                .copy_from_slice(&buf[off + 3..off + 3 + cfg.channels[i].psk_len as usize]);
            cfg.channels[i].name_len = buf[off + 35].min(12);
            cfg.channels[i].name[..cfg.channels[i].name_len as usize]
                .copy_from_slice(&buf[off + 36..off + 36 + cfg.channels[i].name_len as usize]);
        }

        info!(
            "[NVS] Config loaded: region={} preset={} channels={}",
            cfg.region, cfg.modem_preset, cfg.num_channels
        );
        Some(cfg)
    }

    /// Save device config to flash sector 0 of the NVS partition.
    pub fn save_config(&mut self, cfg: &SavedConfig) {
        let base = self.nvs_offset + CONFIG_OFFSET;
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

        if let Err(e) = self.flash.write(base, &buf) {
            error!("[NVS] Config write failed: {:?}", e);
        } else {
            info!("[NVS] Config saved");
        }
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
