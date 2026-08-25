//! NVS Storage Adapter — persistent radio frame storage and device config,
//! backed by `mpsl::Flash` (writes/erases arbitrated against radio timeslots).
//!
//! Flash layout within the 5-sector NVS region at 0xEF000 (see `memory.x`),
//! each sector 4096 bytes:
//!   Sector 0 (+0x0000): Device config (SavedConfig, 512 bytes at offset 0)
//!   Sector 1 (+0x1000): BLE bond data (48 bytes at offset 0)
//!   Sector 2 (+0x2000): Message ring buffer (64-byte header + 10 × 260-byte slots)
//!   Sector 3 (+0x3000): NodeDB snapshot
//!   Sector 4 (+0x4000): X25519 PKC keypair (72-byte blob)
//!
//! This mirrors the ESP32 adapter's layout exactly, offset from a different
//! base. Record layouts (magic numbers, versions, field offsets) live in
//! `meshtastenstein_core::domain::persistence` — shared with every board.
//! This file is only the flash-offset bookkeeping and the async `mpsl::Flash`
//! I/O around those functions.

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
use nrf_mpsl::Flash;

/// Base offset of the NVS region within the chip's flash, per `memory.x`.
const NVS_BASE: u32 = 0xEF000;

const CONFIG_OFFSET: u32 = NVS_BASE;
const BOND_OFFSET: u32 = NVS_BASE + 0x1000;
const STORAGE_OFFSET: u32 = NVS_BASE + 0x2000;
const NODEDB_OFFSET: u32 = NVS_BASE + 0x3000;
const PKC_OFFSET: u32 = NVS_BASE + 0x4000;

const SECTOR_SIZE: u32 = 0x1000;

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

pub struct NrfNvmcStorageAdapter<'d> {
    head: usize,
    tail: usize,
    count: usize,
    slots: [CachedSlot; MAX_BUFFERED_MESSAGES],
    flash: Flash<'d>,
    dirty: bool,
}

impl<'d> NrfNvmcStorageAdapter<'d> {
    /// `flash` must come from `nrf_mpsl::Flash::take(mpsl, p.NVMC)`, called
    /// after MPSL is initialized with timeslot support.
    pub async fn new(flash: Flash<'d>) -> Self {
        let mut adapter = Self {
            head: 0,
            tail: 0,
            count: 0,
            slots: [CachedSlot::default(); MAX_BUFFERED_MESSAGES],
            flash,
            dirty: false,
        };
        adapter.load_or_init().await;
        adapter
    }

    async fn load_or_init(&mut self) {
        let mut header = [0u8; RING_HEADER_SIZE];
        if self.flash.read(STORAGE_OFFSET, &mut header).is_err() {
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

        self.load_slots();
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

    fn load_config(&mut self) -> Option<persistence::SavedConfig> {
        let mut buf = [0u8; CONFIG_SIZE];
        if self.flash.read(CONFIG_OFFSET, &mut buf).is_err() {
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

    async fn save_config(&mut self, cfg: &persistence::SavedConfig) -> Result<(), StorageError> {
        if let Err(e) = self
            .flash
            .erase(CONFIG_OFFSET, CONFIG_OFFSET + SECTOR_SIZE)
            .await
        {
            error!("[NVS] Config erase failed: {:?}", e);
            return Err(StorageError::StorageError);
        }

        let buf = persistence::encode_config(cfg);
        if let Err(e) = self.flash.write(CONFIG_OFFSET, &buf).await {
            error!("[NVS] Config write failed: {:?}", e);
            return Err(StorageError::StorageError);
        }
        info!("[NVS] Config saved");
        Ok(())
    }

    async fn erase_config_internal(&mut self) {
        if let Err(e) = self
            .flash
            .erase(CONFIG_OFFSET, CONFIG_OFFSET + SECTOR_SIZE)
            .await
        {
            error!("[NVS] Config erase failed: {:?}", e);
        } else {
            info!("[NVS] Config erased (factory reset)");
        }
    }

    fn load_bond_internal(&mut self) -> Option<[u8; BOND_SIZE]> {
        let mut buf = [0u8; BOND_SIZE];
        if self.flash.read(BOND_OFFSET, &mut buf).is_err() {
            return None;
        }
        if !persistence::bond_magic_valid(&buf) {
            return None;
        }
        info!("[NVS] Bond loaded from flash");
        Some(buf)
    }

    async fn save_bond_internal(&mut self, bytes: &[u8; BOND_SIZE]) -> Result<(), StorageError> {
        if let Err(e) = self
            .flash
            .erase(BOND_OFFSET, BOND_OFFSET + SECTOR_SIZE)
            .await
        {
            error!("[NVS] Bond erase failed: {:?}", e);
            return Err(StorageError::StorageError);
        }

        if let Err(e) = self.flash.write(BOND_OFFSET, bytes).await {
            error!("[NVS] Bond write failed: {:?}", e);
            return Err(StorageError::StorageError);
        }
        info!("[NVS] Bond saved to flash");
        Ok(())
    }

    async fn clear_bond_internal(&mut self) {
        if let Err(e) = self
            .flash
            .erase(BOND_OFFSET, BOND_OFFSET + SECTOR_SIZE)
            .await
        {
            error!("[NVS] Bond erase failed: {:?}", e);
        } else {
            info!("[NVS] Bond cleared");
        }
    }

    fn load_node_db_internal(&mut self) -> Option<[u8; SNAPSHOT_BYTES]> {
        let mut buf = [0u8; SNAPSHOT_BYTES];
        if self.flash.read(NODEDB_OFFSET, &mut buf).is_err() {
            warn!("[NVS] NodeDB snapshot read failed");
            return None;
        }
        Some(buf)
    }

    async fn save_node_db_internal(
        &mut self,
        bytes: &[u8; SNAPSHOT_BYTES],
    ) -> Result<(), StorageError> {
        if let Err(e) = self
            .flash
            .erase(NODEDB_OFFSET, NODEDB_OFFSET + SECTOR_SIZE)
            .await
        {
            error!("[NVS] NodeDB erase failed: {:?}", e);
            return Err(StorageError::StorageError);
        }

        if let Err(e) = self.flash.write(NODEDB_OFFSET, bytes).await {
            error!("[NVS] NodeDB write failed: {:?}", e);
            return Err(StorageError::StorageError);
        }
        info!("[NVS] NodeDB snapshot saved ({} bytes)", bytes.len());
        Ok(())
    }

    async fn erase_node_db_internal(&mut self) {
        if let Err(e) = self
            .flash
            .erase(NODEDB_OFFSET, NODEDB_OFFSET + SECTOR_SIZE)
            .await
        {
            error!("[NVS] NodeDB erase failed: {:?}", e);
        } else {
            info!("[NVS] NodeDB snapshot erased");
        }
    }

    fn load_pkc_keypair_internal(&mut self) -> Option<([u8; 32], [u8; 32])> {
        let mut buf = [0u8; PKC_BLOB_SIZE];
        if self.flash.read(PKC_OFFSET, &mut buf).is_err() {
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

    async fn save_pkc_keypair_internal(
        &mut self,
        priv_key: &[u8; 32],
        pub_key: &[u8; 32],
    ) -> Result<(), StorageError> {
        if let Err(e) = self.flash.erase(PKC_OFFSET, PKC_OFFSET + SECTOR_SIZE).await {
            error!("[NVS] PKC keypair erase failed: {:?}", e);
            return Err(StorageError::StorageError);
        }

        let buf = persistence::encode_pkc_keypair(priv_key, pub_key);
        if let Err(e) = self.flash.write(PKC_OFFSET, &buf).await {
            error!("[NVS] PKC keypair write failed: {:?}", e);
            return Err(StorageError::StorageError);
        }
        info!("[NVS] PKC keypair saved");
        Ok(())
    }

    fn slot_flash_offset(&self, index: usize) -> u32 {
        STORAGE_OFFSET + (RING_HEADER_SIZE + index * SLOT_FLASH_SIZE) as u32
    }

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

    async fn persist_header(&mut self) {
        let header = persistence::encode_ring_header(&RingHeader {
            head: self.head,
            tail: self.tail,
            count: self.count,
        });

        if let Err(e) = self.flash.write(STORAGE_OFFSET, &header).await {
            error!("[NVS] Failed to write header: {:?}", e);
        }
        self.dirty = false;
    }
}

impl<'d> StorageTrait for NrfNvmcStorageAdapter<'d> {
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

impl<'d> ConfigStorage for NrfNvmcStorageAdapter<'d> {
    async fn save_state(&mut self, device: &DeviceState) -> Result<(), StorageError> {
        let cfg = persistence::saved_config_from_device(device);
        self.save_config(&cfg).await
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
        self.save_bond_internal(bytes).await
    }

    async fn load_bond(&mut self) -> Option<[u8; 48]> {
        self.load_bond_internal()
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
        self.save_pkc_keypair_internal(priv_key, pub_key).await
    }
}
