//! NVS Storage Adapter — type alias binding the shared
//! `meshtastenstein_core::adapters::GenericNvsStorage` to `nrf_mpsl::Flash`.
//!
//! `nrf_mpsl::Flash` already implements the async
//! `embedded_storage_async::nor_flash::NorFlash` directly (writes/erases are
//! arbitrated against radio timeslots, hence async), so no shim is needed
//! here unlike the ESP32 board. Flash offset bookkeeping, record layouts,
//! and I/O sequencing all live in the shared adapter; this file only
//! supplies the base offset of the NVS region within this chip's flash.

use nrf_mpsl::Flash;

use meshtastenstein_core::adapters::GenericNvsStorage;

/// Base offset of the NVS region within the chip's flash, per `memory.x`.
pub const NVS_BASE: u32 = 0xEF000;

pub type NrfNvmcStorageAdapter<'d> = GenericNvsStorage<Flash<'d>>;
