//! `Sleep` port implementation for nRF52840, backed by System Off mode.
//!
//! Unlike the ESP32 board's deep sleep, this has no GPIO wakeup source —
//! upstream's own nRF52 port doesn't wire one either outside a
//! `BATTERY_LPCOMP_INPUT` (a charge-detect wakeup irrelevant to this board);
//! it treats System Off as the terminal power state and relies on the reset
//! button or power cycle to wake. Matching that behaviour rather than
//! inventing a wake-on-LoRa path this board doesn't support in hardware.

use embassy_nrf::power;
use log::info;
use meshtastenstein_core::ports::Sleep;

pub struct NrfSleepAdapter;

impl Sleep for NrfSleepAdapter {
    fn enter_sleep(&mut self) -> ! {
        info!("[Sleep] Entering System Off");
        power::set_system_off();
        // System Off cuts CPU power; execution does not continue past the
        // register write above on real hardware. This loop exists only to
        // satisfy the diverging return type.
        loop {
            cortex_m::asm::wfe();
        }
    }
}
