//! NVS Storage Adapter - Persistent radio frame storage using flash memory

use crate::constants::{MAX_BUFFERED_MESSAGES, MAX_LORA_PAYLOAD_LEN};
use crate::mesh::packet::RadioFrame;
use crate::ports::{Storage as StorageTrait, StorageError};
use embedded_storage::{ReadStorage, Storage};
use esp_bootloader_esp_idf::partitions::{self, DataPartitionSubType, PartitionType};
use esp_storage::FlashStorage;
use log::{error, info};

const STORAGE_OFFSET: u32 = 0x1000;
const HEADER_SIZE: usize = 64;
const MAGIC: u32 = 0x4D455348; // "MESH"

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
