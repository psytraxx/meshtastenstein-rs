//! Shared NVS storage adapter, generic over any `embedded_storage_async::NorFlash`.
//!
//! Both boards need the same five-sector flash layout (device config, BLE
//! bond, message ring buffer, NodeDB snapshot, X25519 PKC keypair) and the
//! same erase-then-write orchestration around it — only the concrete flash
//! type and its base offset within the chip's flash differ. Record layouts
//! (magic numbers, versions, field offsets) live in `domain::persistence`;
//! this module is the flash-offset bookkeeping and I/O sequencing around
//! those functions, shared by every board through the `NorFlash` bound.
//!
//! `F: NorFlash` (the *async* trait) is satisfiable by a genuinely async
//! flash driver (nRF52's `mpsl::Flash`, which must arbitrate against radio
//! timeslots) or by a synchronous one wrapped in a thin shim whose method
//! bodies never yield (ESP32's `esp_storage::FlashStorage`) — see
//! `ports::ConfigStorage`'s doc comment for the same reasoning applied to
//! the port trait itself.

use embedded_storage_async::nor_flash::NorFlash;
use log::{error, info, warn};

use crate::{
    constants::{MAX_BUFFERED_MESSAGES, MAX_LORA_PAYLOAD_LEN},
    domain::{
        device::DeviceState,
        node_db::{NodeDB, SNAPSHOT_BYTES},
        packet::RadioFrame,
        persistence::{self, BOND_SIZE, CONFIG_SIZE, PKC_BLOB_SIZE, RING_HEADER_SIZE, RingHeader},
    },
    ports::{ConfigStorage, Storage as StorageTrait, StorageError},
};

const SECTOR_SIZE: u32 = 0x1000;

const CONFIG_SECTOR: u32 = 0;
const BOND_SECTOR: u32 = 1;
const STORAGE_SECTOR: u32 = 2;
const NODEDB_SECTOR: u32 = 3;
const PKC_SECTOR: u32 = 4;

/// Per-slot on-flash size: 1 (valid flag) + 1 (len) + 255 (data) + 3 (padding) = 260.
/// 10 slots × 260 = 2600 bytes; header = 64 bytes; total = 2664 < 4096 (sector).
const SLOT_FLASH_SIZE: usize = 260;

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

/// NVS storage adapter generic over any `NorFlash` implementation.
///
/// Flash layout within the NVS region (each sector 4096 bytes, relative to
/// `base_offset`):
///   Sector 0 (+0x0000): Device config (SavedConfig, 512 bytes at offset 0)
///   Sector 1 (+0x1000): BLE bond data (48 bytes at offset 0)
///   Sector 2 (+0x2000): Message ring buffer (64-byte header + 10 × 260-byte slots)
///   Sector 3 (+0x3000): NodeDB snapshot
///   Sector 4 (+0x4000): X25519 PKC keypair (72-byte blob)
pub struct GenericNvsStorage<F: NorFlash> {
    head: usize,
    tail: usize,
    count: usize,
    slots: [CachedSlot; MAX_BUFFERED_MESSAGES],
    flash: F,
    base_offset: u32,
    dirty: bool,
}

impl<F: NorFlash> GenericNvsStorage<F> {
    /// `base_offset` is the start of the 5-sector NVS region within the
    /// chip's flash (board-specific: a runtime-discovered partition offset
    /// on ESP32, a fixed linker-script constant on nRF52).
    pub async fn new(flash: F, base_offset: u32) -> Self {
        let mut adapter = Self {
            head: 0,
            tail: 0,
            count: 0,
            slots: [CachedSlot::default(); MAX_BUFFERED_MESSAGES],
            flash,
            base_offset,
            dirty: false,
        };
        adapter.load_or_init().await;
        adapter
    }

    fn sector_offset(&self, sector: u32) -> u32 {
        self.base_offset + sector * SECTOR_SIZE
    }

    async fn load_or_init(&mut self) {
        let base = self.sector_offset(STORAGE_SECTOR);
        let mut header = [0u8; RING_HEADER_SIZE];
        if self.flash.read(base, &mut header).await.is_err() {
            self.init_empty().await;
            return;
        }

        let Some(RingHeader { head, tail, count }) =
            persistence::decode_ring_header(&header, MAX_BUFFERED_MESSAGES)
        else {
            info!("[NVS] Fresh storage, initializing...");
            self.init_empty().await;
            return;
        };
        self.head = head;
        self.tail = tail;
        self.count = count;

        // Restore slot data from flash so peek() works after a reboot or wake from sleep.
        self.load_slots().await;
        info!("[NVS] Restored: {} buffered frames", self.count);
    }

    async fn init_empty(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.count = 0;
        self.slots = [CachedSlot::default(); MAX_BUFFERED_MESSAGES];
        self.dirty = true;
        self.persist_header().await;
    }

    /// Load device config from flash sector 0. Returns `None` if magic is
    /// wrong (first boot or corrupted).
    async fn load_config(&mut self) -> Option<persistence::SavedConfig> {
        let base = self.sector_offset(CONFIG_SECTOR);
        let mut buf = [0u8; CONFIG_SIZE];
        if self.flash.read(base, &mut buf).await.is_err() {
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

    /// Save device config to flash sector 0. Erases the sector first (NOR
    /// flash requirement: cannot set bits 0→1 without erase).
    async fn save_config(&mut self, cfg: &persistence::SavedConfig) -> Result<(), StorageError> {
        let base = self.sector_offset(CONFIG_SECTOR);

        if let Err(e) = self.flash.erase(base, base + SECTOR_SIZE).await {
            error!("[NVS] Config erase failed: {:?}", e);
            return Err(StorageError::StorageError);
        }

        let buf = persistence::encode_config(cfg);
        if let Err(e) = self.flash.write(base, &buf).await {
            error!("[NVS] Config write failed: {:?}", e);
            return Err(StorageError::StorageError);
        }
        info!("[NVS] Config saved");
        Ok(())
    }

    /// Erase device config from flash (factory reset).
    async fn erase_config_internal(&mut self) {
        let base = self.sector_offset(CONFIG_SECTOR);
        if let Err(e) = self.flash.erase(base, base + SECTOR_SIZE).await {
            error!("[NVS] Config erase failed: {:?}", e);
        } else {
            info!("[NVS] Config erased (factory reset)");
        }
    }

    /// Load BLE bond from flash. Returns raw 48-byte blob or None if absent/corrupt.
    async fn load_bond_internal(&mut self) -> Option<[u8; BOND_SIZE]> {
        let base = self.sector_offset(BOND_SECTOR);
        let mut buf = [0u8; BOND_SIZE];
        if self.flash.read(base, &mut buf).await.is_err() {
            return None;
        }
        if !persistence::bond_magic_valid(&buf) {
            return None;
        }
        info!("[NVS] Bond loaded from flash");
        Some(buf)
    }

    /// Save BLE bond to flash (48-byte raw blob from BLE task). Erases the
    /// bond sector first (NOR flash requirement).
    async fn save_bond_internal(&mut self, bytes: &[u8; BOND_SIZE]) -> Result<(), StorageError> {
        let base = self.sector_offset(BOND_SECTOR);

        if let Err(e) = self.flash.erase(base, base + SECTOR_SIZE).await {
            error!("[NVS] Bond erase failed: {:?}", e);
            return Err(StorageError::StorageError);
        }

        if let Err(e) = self.flash.write(base, bytes).await {
            error!("[NVS] Bond write failed: {:?}", e);
            return Err(StorageError::StorageError);
        }
        info!("[NVS] Bond saved to flash");
        Ok(())
    }

    /// Erase the stored bond (e.g. on pairing failure or explicit clear).
    async fn clear_bond_internal(&mut self) {
        let base = self.sector_offset(BOND_SECTOR);
        if let Err(e) = self.flash.erase(base, base + SECTOR_SIZE).await {
            error!("[NVS] Bond erase failed: {:?}", e);
        } else {
            info!("[NVS] Bond cleared");
        }
    }

    /// Load the raw NodeDB snapshot blob from sector 3. Returns `None` when
    /// the sector is blank (first boot) or the read fails.
    async fn load_node_db_internal(&mut self) -> Option<[u8; SNAPSHOT_BYTES]> {
        let base = self.sector_offset(NODEDB_SECTOR);
        let mut buf = [0u8; SNAPSHOT_BYTES];
        if self.flash.read(base, &mut buf).await.is_err() {
            warn!("[NVS] NodeDB snapshot read failed");
            return None;
        }
        // Magic check is owned by the codec inside `NodeDB::restore_snapshot`.
        // We just hand the raw bytes back.
        Some(buf)
    }

    /// Persist a raw NodeDB snapshot blob to sector 3. Erases the sector
    /// first (NOR flash requirement).
    async fn save_node_db_internal(
        &mut self,
        bytes: &[u8; SNAPSHOT_BYTES],
    ) -> Result<(), StorageError> {
        let base = self.sector_offset(NODEDB_SECTOR);

        if let Err(e) = self.flash.erase(base, base + SECTOR_SIZE).await {
            error!("[NVS] NodeDB erase failed: {:?}", e);
            return Err(StorageError::StorageError);
        }

        if let Err(e) = self.flash.write(base, bytes).await {
            error!("[NVS] NodeDB write failed: {:?}", e);
            return Err(StorageError::StorageError);
        }
        info!("[NVS] NodeDB snapshot saved ({} bytes)", bytes.len());
        Ok(())
    }

    /// Erase the NodeDB snapshot sector (factory reset).
    async fn erase_node_db_internal(&mut self) {
        let base = self.sector_offset(NODEDB_SECTOR);
        if let Err(e) = self.flash.erase(base, base + SECTOR_SIZE).await {
            error!("[NVS] NodeDB erase failed: {:?}", e);
        } else {
            info!("[NVS] NodeDB snapshot erased");
        }
    }

    /// Load the X25519 keypair from sector 4. Returns `None` on first boot
    /// (blank flash) or if the sector magic is wrong / version unsupported.
    async fn load_pkc_keypair_internal(&mut self) -> Option<([u8; 32], [u8; 32])> {
        let base = self.sector_offset(PKC_SECTOR);
        let mut buf = [0u8; PKC_BLOB_SIZE];
        if self.flash.read(base, &mut buf).await.is_err() {
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
    async fn save_pkc_keypair_internal(
        &mut self,
        priv_key: &[u8; 32],
        pub_key: &[u8; 32],
    ) -> Result<(), StorageError> {
        let base = self.sector_offset(PKC_SECTOR);

        if let Err(e) = self.flash.erase(base, base + SECTOR_SIZE).await {
            error!("[NVS] PKC keypair erase failed: {:?}", e);
            return Err(StorageError::StorageError);
        }

        let buf = persistence::encode_pkc_keypair(priv_key, pub_key);
        if let Err(e) = self.flash.write(base, &buf).await {
            error!("[NVS] PKC keypair write failed: {:?}", e);
            return Err(StorageError::StorageError);
        }
        info!("[NVS] PKC keypair saved");
        Ok(())
    }

    fn slot_flash_offset(&self, index: usize) -> u32 {
        self.sector_offset(STORAGE_SECTOR) + (RING_HEADER_SIZE + index * SLOT_FLASH_SIZE) as u32
    }

    /// Write one slot to flash. Called by `add()` and `pop()`.
    async fn persist_slot(&mut self, index: usize) {
        let mut buf = [0u8; SLOT_FLASH_SIZE];
        let slot = &self.slots[index];
        persistence::encode_ring_slot_prefix(&mut buf, slot.valid, slot.len);
        if slot.valid && slot.len > 0 {
            buf[2..2 + slot.len].copy_from_slice(&slot.data[..slot.len]);
        }
        let offset = self.slot_flash_offset(index);
        if let Err(e) = self.flash.write(offset, &buf).await {
            error!("[NVS] Failed to write slot {}: {:?}", index, e);
        }
    }

    /// Read all slots from flash into RAM. Called once by `load_or_init()`.
    async fn load_slots(&mut self) {
        let mut buf = [0u8; SLOT_FLASH_SIZE];
        for i in 0..MAX_BUFFERED_MESSAGES {
            let offset = self.slot_flash_offset(i);
            if self.flash.read(offset, &mut buf).await.is_err() {
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

    async fn persist_header(&mut self) {
        let base = self.sector_offset(STORAGE_SECTOR);
        let header = persistence::encode_ring_header(&RingHeader {
            head: self.head,
            tail: self.tail,
            count: self.count,
        });

        if let Err(e) = self.flash.write(base, &header).await {
            error!("[NVS] Failed to write header: {:?}", e);
        }
        self.dirty = false;
    }
}

impl<F: NorFlash> StorageTrait for GenericNvsStorage<F> {
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
        self.persist_slot(slot_idx).await;
        self.persist_header().await;
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
        self.persist_slot(old_tail).await;
        self.persist_header().await;
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
        self.persist_header().await;
    }
}

impl<F: NorFlash> ConfigStorage for GenericNvsStorage<F> {
    async fn save_state(&mut self, device: &DeviceState) -> Result<(), StorageError> {
        let cfg = persistence::saved_config_from_device(device);
        self.save_config(&cfg).await
    }

    async fn load_state(&mut self, device: &mut DeviceState) {
        let Some(saved) = self.load_config().await else {
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
        self.save_bond_internal(bytes).await
    }

    async fn load_bond(&mut self) -> Option<[u8; 48]> {
        self.load_bond_internal().await
    }

    async fn clear_bond(&mut self) {
        self.clear_bond_internal().await;
    }

    async fn erase_config(&mut self) {
        self.erase_config_internal().await;
        // Factory reset wipes node DB too: the new owner shouldn't inherit a
        // stranger's mesh history.
        self.erase_node_db_internal().await;
    }

    async fn save_node_db(&mut self, db: &NodeDB) -> Result<(), StorageError> {
        let snapshot = db.to_snapshot();
        self.save_node_db_internal(&snapshot).await
    }

    async fn load_node_db(&mut self, db: &mut NodeDB) {
        if let Some(buf) = self.load_node_db_internal().await
            && db.restore_snapshot(&buf)
        {
            info!("[NVS] NodeDB restored: {} node(s)", db.len());
        }
    }

    async fn load_pkc_keypair(&mut self) -> Option<([u8; 32], [u8; 32])> {
        self.load_pkc_keypair_internal().await
    }

    async fn save_pkc_keypair(
        &mut self,
        priv_key: &[u8; 32],
        pub_key: &[u8; 32],
    ) -> Result<(), StorageError> {
        self.save_pkc_keypair_internal(priv_key, pub_key).await
    }
}
