/* Seeed XIAO nRF52840 — 1 MB flash, 256 KB RAM.
 *
 * The board ships with the Adafruit UF2 bootloader, which constrains both ends
 * of flash and must not be overwritten (doing so costs you USB drag-and-drop
 * flashing and needs a SWD probe to recover):
 *
 *   0x00000 - 0x27000   MBR + SoftDevice region. We use nrf-sdc rather than
 *                       Nordic's S140 SoftDevice, so nothing of ours lives
 *                       here, but the bootloader still expects the
 *                       application to start at 0x27000.
 *   0x27000 - 0xEF000   Application (this firmware).
 *   0xEF000 - 0xF4000   NVS: 5 × 4 KB sectors, see nrf_nvmc_storage_adapter.rs.
 *   0xF4000 - 0x100000  Bootloader, its config, MBR params and settings pages.
 *
 * RAM: nrf-sdc/MPSL claim a chunk at runtime from the pool we hand them rather
 * than a link-time carve-out, so RAM is declared in full here.
 */
MEMORY
{
  FLASH : ORIGIN = 0x00027000, LENGTH = 0xC8000  /* 800 KB, ends at 0xEF000 */
  RAM   : ORIGIN = 0x20000000, LENGTH = 256K
}
