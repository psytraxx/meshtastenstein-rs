//! NVS Storage Adapter - ESP-IDF partition lookup + sync-to-async flash shim.
//!
//! Flash offset bookkeeping, record layouts, and I/O sequencing all live in
//! `meshtastenstein_core::adapters::GenericNvsStorage` (shared with every
//! board). This file only discovers where the NVS partition sits (ESP-IDF
//! specific — no nRF52 analog) and wraps `esp_storage::FlashStorage`'s
//! synchronous `embedded_storage` API so it satisfies the shared adapter's
//! `embedded_storage_async::nor_flash::NorFlash` bound.

use embedded_storage_async::nor_flash::{ErrorType, NorFlash, ReadNorFlash};
use esp_bootloader_esp_idf::partitions::{self, DataPartitionSubType, PartitionType};
use esp_storage::FlashStorage;
use log::info;
use meshtastenstein_core::adapters::GenericNvsStorage;

/// Wraps `esp_storage::FlashStorage`'s synchronous `embedded_storage` API to
/// satisfy `GenericNvsStorage`'s async `NorFlash` bound. Every method body
/// calls straight through to the sync implementation and never yields — the
/// same pattern `ports::ConfigStorage`'s doc comment already sanctions for a
/// fully synchronous board.
pub(crate) struct SyncNorFlashAdapter<'a>(FlashStorage<'a>);

impl<'a> ErrorType for SyncNorFlashAdapter<'a> {
    type Error = <FlashStorage<'a> as embedded_storage::nor_flash::ErrorType>::Error;
}

impl<'a> ReadNorFlash for SyncNorFlashAdapter<'a> {
    const READ_SIZE: usize = 1;

    async fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        embedded_storage::ReadStorage::read(&mut self.0, offset, bytes)
    }

    fn capacity(&self) -> usize {
        embedded_storage::ReadStorage::capacity(&self.0)
    }
}

impl<'a> NorFlash for SyncNorFlashAdapter<'a> {
    const WRITE_SIZE: usize = 1;
    const ERASE_SIZE: usize = 0x1000;

    async fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        embedded_storage::nor_flash::NorFlash::erase(&mut self.0, from, to)
    }

    async fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        embedded_storage::Storage::write(&mut self.0, offset, bytes)
    }
}

pub type NvsStorageAdapter<'a> = GenericNvsStorage<SyncNorFlashAdapter<'a>>;

/// Discover the NVS partition offset from the ESP-IDF partition table, then
/// build the shared adapter on top of it.
pub async fn new_nvs_storage_adapter(
    flash_peripheral: esp_hal::peripherals::FLASH<'_>,
) -> NvsStorageAdapter<'_> {
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

    GenericNvsStorage::new(SyncNorFlashAdapter(flash), nvs_offset).await
}
