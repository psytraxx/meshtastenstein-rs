//! NVS Storage Adapter - Persistent radio frame storage and device config using flash memory
//!
//! Flash layout within the NVS partition (each sector = 4096 bytes):
//!   Sector 0 (0x0000–0x0FFF): Device config (SavedConfig, 512 bytes at offset 0)
//!   Sector 1 (0x1000–0x1FFF): BLE bond data (48 bytes at offset 0)
//!   Sector 2 (0x2000–0x2FFF): Message ring buffer (64-byte header + 10 × 260-byte slots;
//!                             slots are mirrored in RAM but persisted to flash on add/pop)
//!   Sector 3 (0x3000–0x3FFF): NodeDB snapshot (Phase 2 G4)
//!   Sector 4 (0x4000–0x4FFF): X25519 PKC keypair (Phase 2 G2, 72-byte blob)
//!
//! Each sector is erased before write (NOR flash: bits can only go 1→0 without erase).
//!
//! Record layouts (magic numbers, versions, field offsets) live in
//! `meshtastenstein_core::domain::persistence` — shared with every board,
//! since they're pure encode/decode with no flash dependency. This file is
//! only the ESP-IDF partition lookup and the synchronous `embedded_storage`
//! I/O around those functions.

use embedded_storage::{ReadStorage, Storage};
use esp_bootloader_esp_idf::partitions::{self, DataPartitionSubType, PartitionType};
use esp_storage::FlashStorage;
use log::{error, info, warn};
use meshtastenstein_core::{
    constants::{MAX_BUFFERED_MESSAGES, MAX_LORA_PAYLOAD_LEN},
    domain::{
        device::DeviceState,
        node_db::{NodeDB, SNAPSHOT_BYTES},
        packet::RadioFrame,
        persistence::{self, BOND_SIZE, CONFIG_SIZE, PKC_BLOB_SIZE, RING_HEADER_SIZE, RingHeader},
    },
    ports::{ConfigStorage, Storage as StorageTrait, StorageError},
};

const STORAGE_OFFSET: u32 = 0x2000;

/// Per-slot on-flash size: 1 (valid flag) + 1 (len) + 255 (data) + 3 (padding) = 260.
/// 10 slots × 260 = 2600 bytes; header = 64 bytes; total = 2664 < 4096 (sector).
const SLOT_FLASH_SIZE: usize = 260;

// ── Device config persistence ──────────────────────────────────────────────

/// Offset within NVS partition for device config (sector 0)
const CONFIG_OFFSET: u32 = 0x0000;

// Bond storage in its own sector (sector 1) to allow independent erase/write
const BOND_OFFSET: u32 = 0x1000;

// NodeDB snapshot in sector 3. Layout owned by `domain::node_db` —
// the adapter is just a flash backing store.
const NODEDB_OFFSET: u32 = 0x3000;

// X25519 PKC keypair in sector 4 (Phase 2 G2). Own sector so we can
// generate-and-flush exactly once on first boot without read-modify-writing
// the device config sector.
const PKC_OFFSET: u32 = 0x4000;

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
        let mut header = [0u8; RING_HEADER_SIZE];
        let base = self.storage_base();

        if self.flash.read(base, &mut header).is_err() {
            self.init_empty();
            return;
        }

        let Some(RingHeader { head, tail, count }) =
            persistence::decode_ring_header(&header, MAX_BUFFERED_MESSAGES)
        else {
            info!("[NVS] Fresh storage, initializing...");
            self.init_empty();
            return;
        };
        self.head = head;
        self.tail = tail;
        self.count = count;

        // Restore slot data from flash so peek() works after a reboot or wake from sleep.
        self.load_slots();
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
    fn load_config(&mut self) -> Option<persistence::SavedConfig> {
        let base = self.nvs_offset + CONFIG_OFFSET;
        let mut buf = [0u8; CONFIG_SIZE];
        if self.flash.read(base, &mut buf).is_err() {
            warn!("[NVS] Config read failed");
            return None;
        }
        let cfg = persistence::decode_config(&buf);
        if let Some(cfg) = &cfg {
            info!(
                "[NVS] Config loaded: region={} preset={} use_preset={} channels={}",
                cfg.region, cfg.modem_preset, cfg.use_preset, cfg.num_channels
            );
        } else {
            info!("[NVS] No saved config (first boot or unsupported version)");
        }
        cfg
    }

    /// Save device config to flash sector 0 of the NVS partition.
    /// Erases the sector first (NOR flash requirement: cannot set bits 0→1 without erase).
    fn save_config(&mut self, cfg: &persistence::SavedConfig) -> Result<(), StorageError> {
        let base = self.nvs_offset + CONFIG_OFFSET;

        // Erase the config sector before writing (4096-byte sector, NOR flash requirement)
        if let Err(e) =
            embedded_storage::nor_flash::NorFlash::erase(&mut self.flash, base, base + 0x1000)
        {
            error!("[NVS] Config erase failed: {:?}", e);
            return Err(StorageError::StorageError);
        }

        let buf = persistence::encode_config(cfg);
        if let Err(e) = self.flash.write(base, &buf) {
            error!("[NVS] Config write failed: {:?}", e);
            return Err(StorageError::StorageError);
        }
        info!("[NVS] Config saved");
        Ok(())
    }

    /// Erase device config from flash (factory reset).
    fn erase_config_internal(&mut self) {
        let base = self.nvs_offset + CONFIG_OFFSET;
        if let Err(e) =
            embedded_storage::nor_flash::NorFlash::erase(&mut self.flash, base, base + 0x1000)
        {
            error!("[NVS] Config erase failed: {:?}", e);
        } else {
            info!("[NVS] Config erased (factory reset)");
        }
    }

    /// Load BLE bond from flash. Returns raw 48-byte blob or None if absent/corrupt.
    fn load_bond_internal(&mut self) -> Option<[u8; BOND_SIZE]> {
        let base = self.nvs_offset + BOND_OFFSET;
        let mut buf = [0u8; BOND_SIZE];
        if self.flash.read(base, &mut buf).is_err() {
            return None;
        }
        if !persistence::bond_magic_valid(&buf) {
            return None;
        }
        info!("[NVS] Bond loaded from flash");
        Some(buf)
    }

    /// Save BLE bond to flash (48-byte raw blob from BLE task).
    /// Erases the bond sector first (NOR flash requirement).
    fn save_bond_internal(&mut self, bytes: &[u8; BOND_SIZE]) -> Result<(), StorageError> {
        let base = self.nvs_offset + BOND_OFFSET;

        // Erase the bond sector before writing (4096-byte sector, NOR flash requirement)
        if let Err(e) =
            embedded_storage::nor_flash::NorFlash::erase(&mut self.flash, base, base + 0x1000)
        {
            error!("[NVS] Bond erase failed: {:?}", e);
            return Err(StorageError::StorageError);
        }

        if let Err(e) = self.flash.write(base, bytes) {
            error!("[NVS] Bond write failed: {:?}", e);
            return Err(StorageError::StorageError);
        }
        info!("[NVS] Bond saved to flash");
        Ok(())
    }

    /// Load the raw NodeDB snapshot blob from sector 3. Returns `None` when
    /// the sector is blank (first boot) or the read fails.
    fn load_node_db_internal(&mut self) -> Option<[u8; SNAPSHOT_BYTES]> {
        let base = self.nvs_offset + NODEDB_OFFSET;
        let mut buf = [0u8; SNAPSHOT_BYTES];
        if self.flash.read(base, &mut buf).is_err() {
            warn!("[NVS] NodeDB snapshot read failed");
            return None;
        }
        // Magic check is owned by the codec inside `NodeDB::restore_snapshot`.
        // We just hand the raw bytes back.
        Some(buf)
    }

    /// Persist a raw NodeDB snapshot blob to sector 3. Erases the sector
    /// first (NOR flash requirement).
    fn save_node_db_internal(&mut self, bytes: &[u8; SNAPSHOT_BYTES]) -> Result<(), StorageError> {
        let base = self.nvs_offset + NODEDB_OFFSET;

        if let Err(e) =
            embedded_storage::nor_flash::NorFlash::erase(&mut self.flash, base, base + 0x1000)
        {
            error!("[NVS] NodeDB erase failed: {:?}", e);
            return Err(StorageError::StorageError);
        }

        if let Err(e) = self.flash.write(base, bytes) {
            error!("[NVS] NodeDB write failed: {:?}", e);
            return Err(StorageError::StorageError);
        }
        info!("[NVS] NodeDB snapshot saved ({} bytes)", bytes.len());
        Ok(())
    }

    /// Erase the NodeDB snapshot sector (factory reset).
    fn erase_node_db_internal(&mut self) {
        let base = self.nvs_offset + NODEDB_OFFSET;
        if let Err(e) =
            embedded_storage::nor_flash::NorFlash::erase(&mut self.flash, base, base + 0x1000)
        {
            error!("[NVS] NodeDB erase failed: {:?}", e);
        } else {
            info!("[NVS] NodeDB snapshot erased");
        }
    }

    /// Load the X25519 keypair from sector 4. Returns `None` on first boot
    /// (blank flash) or if the sector magic is wrong / version unsupported.
    fn load_pkc_keypair_internal(&mut self) -> Option<([u8; 32], [u8; 32])> {
        let base = self.nvs_offset + PKC_OFFSET;
        let mut buf = [0u8; PKC_BLOB_SIZE];
        if self.flash.read(base, &mut buf).is_err() {
            warn!("[NVS] PKC keypair read failed");
            return None;
        }
        let keypair = persistence::decode_pkc_keypair(&buf);
        if keypair.is_some() {
            info!("[NVS] PKC keypair loaded");
        } else {
            info!("[NVS] No PKC keypair on flash (first boot)");
        }
        keypair
    }

    /// Persist an X25519 keypair to sector 4. Erases the sector first.
    fn save_pkc_keypair_internal(
        &mut self,
        priv_key: &[u8; 32],
        pub_key: &[u8; 32],
    ) -> Result<(), StorageError> {
        let base = self.nvs_offset + PKC_OFFSET;

        if let Err(e) =
            embedded_storage::nor_flash::NorFlash::erase(&mut self.flash, base, base + 0x1000)
        {
            error!("[NVS] PKC keypair erase failed: {:?}", e);
            return Err(StorageError::StorageError);
        }

        let buf = persistence::encode_pkc_keypair(priv_key, pub_key);
        if let Err(e) = self.flash.write(base, &buf) {
            error!("[NVS] PKC keypair write failed: {:?}", e);
            return Err(StorageError::StorageError);
        }
        info!("[NVS] PKC keypair saved");
        Ok(())
    }

    /// Erase the stored bond (e.g. on pairing failure or explicit clear).
    fn clear_bond_internal(&mut self) {
        let base = self.nvs_offset + BOND_OFFSET;
        let zeroes = [0u8; BOND_SIZE];
        let _ = self.flash.write(base, &zeroes);
        info!("[NVS] Bond cleared");
    }

    fn slot_flash_offset(&self, index: usize) -> u32 {
        self.storage_base() + (RING_HEADER_SIZE + index * SLOT_FLASH_SIZE) as u32
    }

    /// Write one slot to flash.  Called by `add()` and `pop()`.
    fn persist_slot(&mut self, index: usize) {
        let mut buf = [0u8; SLOT_FLASH_SIZE];
        let slot = &self.slots[index];
        persistence::encode_ring_slot_prefix(&mut buf, slot.valid, slot.len);
        if slot.valid && slot.len > 0 {
            buf[2..2 + slot.len].copy_from_slice(&slot.data[..slot.len]);
        }
        let offset = self.slot_flash_offset(index);
        if let Err(e) = self.flash.write(offset, &buf) {
            error!("[NVS] Failed to write slot {}: {:?}", index, e);
        }
    }

    /// Read all slots from flash into RAM.  Called once by `load_or_init()`.
    fn load_slots(&mut self) {
        let mut buf = [0u8; SLOT_FLASH_SIZE];
        for i in 0..MAX_BUFFERED_MESSAGES {
            let offset = self.slot_flash_offset(i);
            if self.flash.read(offset, &mut buf).is_err() {
                self.slots[i].valid = false;
                continue;
            }
            let (valid, len) = persistence::decode_ring_slot_prefix(&buf);
            if valid && len <= MAX_LORA_PAYLOAD_LEN {
                self.slots[i].valid = true;
                self.slots[i].len = len;
                self.slots[i].data[..len].copy_from_slice(&buf[2..2 + len]);
            } else {
                self.slots[i].valid = false;
            }
        }
    }

    fn persist_header(&mut self) {
        let base = self.storage_base();
        let header = persistence::encode_ring_header(&RingHeader {
            head: self.head,
            tail: self.tail,
            count: self.count,
        });

        if let Err(e) = self.flash.write(base, &header) {
            error!("[NVS] Failed to write header: {:?}", e);
        }
        self.dirty = false;
    }
}

impl<'a> StorageTrait for NvsStorageAdapter<'a> {
    async fn add(&mut self, frame: &RadioFrame) -> Result<(), StorageError> {
        if self.count >= MAX_BUFFERED_MESSAGES {
            self.tail = (self.tail + 1) % MAX_BUFFERED_MESSAGES;
            self.count -= 1;
        }

        let slot_idx = self.head;
        self.slots[slot_idx].valid = true;
        self.slots[slot_idx].len = frame.len;
        self.slots[slot_idx].data[..frame.len].copy_from_slice(&frame.data[..frame.len]);

        self.head = (slot_idx + 1) % MAX_BUFFERED_MESSAGES;
        self.count += 1;
        self.dirty = true;
        self.persist_slot(slot_idx);
        self.persist_header();
        Ok(())
    }

    async fn peek(&mut self) -> Result<Option<RadioFrame>, StorageError> {
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

    async fn pop(&mut self) -> Result<(), StorageError> {
        if self.count == 0 {
            return Err(StorageError::Empty);
        }
        let old_tail = self.tail;
        self.slots[old_tail].valid = false;
        self.tail = (old_tail + 1) % MAX_BUFFERED_MESSAGES;
        self.count -= 1;
        self.dirty = true;
        // Write the invalidated slot and updated header so the consumed message
        // isn't replayed again after a reboot or wake from deep sleep.
        self.persist_slot(old_tail);
        self.persist_header();
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

    async fn clear(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.count = 0;
        self.slots = [CachedSlot::default(); MAX_BUFFERED_MESSAGES];
        self.dirty = true;
        self.persist_header();
    }
}

impl<'a> ConfigStorage for NvsStorageAdapter<'a> {
    async fn save_state(&mut self, device: &DeviceState) -> Result<(), StorageError> {
        let cfg = persistence::saved_config_from_device(device);
        self.save_config(&cfg)
    }

    async fn load_state(&mut self, device: &mut DeviceState) {
        let Some(saved) = self.load_config() else {
            return;
        };
        persistence::apply_saved_config(&saved, device);
        info!(
            "[NVS] Config restored: {} ({}) region={}",
            device.long_name.as_str(),
            device.short_name.as_str(),
            device.region
        );
    }

    async fn save_bond(&mut self, bytes: &[u8; 48]) -> Result<(), StorageError> {
        self.save_bond_internal(bytes)
    }

    async fn load_bond(&mut self) -> Option<[u8; 48]> {
        self.load_bond_internal()
    }

    async fn clear_bond(&mut self) {
        self.clear_bond_internal();
    }

    async fn erase_config(&mut self) {
        self.erase_config_internal();
        // Factory reset wipes node DB too: the new owner shouldn't inherit a
        // stranger's mesh history.
        self.erase_node_db_internal();
    }

    async fn save_node_db(&mut self, db: &NodeDB) -> Result<(), StorageError> {
        let snapshot = db.to_snapshot();
        self.save_node_db_internal(&snapshot)
    }

    async fn load_node_db(&mut self, db: &mut NodeDB) {
        if let Some(buf) = self.load_node_db_internal()
            && db.restore_snapshot(&buf)
        {
            info!("[NVS] NodeDB restored: {} node(s)", db.len());
        }
    }

    async fn load_pkc_keypair(&mut self) -> Option<([u8; 32], [u8; 32])> {
        self.load_pkc_keypair_internal()
    }

    async fn save_pkc_keypair(
        &mut self,
        priv_key: &[u8; 32],
        pub_key: &[u8; 32],
    ) -> Result<(), StorageError> {
        self.save_pkc_keypair_internal(priv_key, pub_key)
    }
}
