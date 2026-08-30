//! `Sleep` port implementation for nRF52840, backed by System Off mode.
//!
//! Unlike the ESP32 board's deep sleep, this has no GPIO wakeup source —
//! upstream's own nRF52 port doesn't wire one either outside a
//! `BATTERY_LPCOMP_INPUT` (a charge-detect wakeup irrelevant to this board);
//! it treats System Off as the terminal power state and relies on the reset
//! button or power cycle to wake. Matching that behaviour rather than
//! inventing a wake-on-LoRa path this board doesn't support in hardware.

use embassy_nrf::{pac, power};
use log::info;
use meshtastenstein_core::ports::Sleep;

/// Same value as upstream's `DFU_MAGIC_SKIP` (`sd_power_gpregret_set`),
/// written into `POWER.GPREGRET` immediately before System Off.
const DFU_MAGIC_SKIP: u8 = 0x6d;

pub struct NrfSleepAdapter;

impl Sleep for NrfSleepAdapter {
    fn enter_sleep(&mut self) -> ! {
        info!("[Sleep] Entering System Off");

        // Upstream clears then sets this register before every
        // sd_power_system_off() call. Without it, the Adafruit UF2
        // bootloader can misread stale retained-register contents on the
        // next reset as a DFU-entry request and sit in the bootloader
        // instead of booting the app — a "device appears bricked after
        // sleep" symptom. GPREGRET survives System Off; the SoftDevice API
        // upstream uses (sd_power_gpregret_clr/set) is unavailable under
        // nrf-sdc/MPSL, so this writes the register directly.
        pac::POWER.gpregret().write(|w| w.set_gpregret(0));
        pac::POWER
            .gpregret()
            .write(|w| w.set_gpregret(DFU_MAGIC_SKIP));

        // NOTE: upstream tears down peripherals (Wire/SPI/Serial/BLE) and
        // explicitly commands the SX1262 to sleep before this point
        // (main-nrf52.cpp:428-440). We don't: `Sleep::enter_sleep` is called
        // from this board's watchdog_task, which has no handle to the radio
        // owned by lora_task — reaching it would need a dedicated shutdown
        // channel/signal into that task, which is real plumbing, not a
        // one-line fix. Until that's added, the SX1262 keeps drawing RX
        // current across System Off, undermining the point of shutting down.
        // Tracked as a known gap rather than silently accepted.
        power::set_system_off();
        // System Off cuts CPU power; execution does not continue past the
        // register write above on real hardware. This loop exists only to
        // satisfy the diverging return type.
        loop {
            cortex_m::asm::wfe();
        }
    }
}
