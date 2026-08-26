//! Deep sleep adapter with GPIO and timer wakeup
//!
//! Known gap vs. upstream: the Heltec V3 has a 32.768 kHz crystal
//! (`variant.h: HAS_32768HZ 1`) that upstream switches the RTC slow-clock to
//! via `enableSlowCLK()` (ESP-IDF's `rtc_clk_32k_enable` + calibration),
//! improving deep-sleep timekeeping accuracy over the internal RC oscillator.
//! `esp-hal` 1.1.2 has no public API for this — it's not exposed anywhere in
//! `rtc_cntl`. Reaching it would mean raw `RTC_CNTL` register pokes
//! reverse-engineered from ESP-IDF's C implementation, which isn't something
//! to do blind without hardware to validate the result against. Left as a
//! documented gap rather than a guessed-at unsafe register hack.

use esp_hal::{
    delay::Delay,
    gpio::{Level, Output, OutputConfig, RtcPin},
    peripherals::{GPIO0, GPIO8, GPIO14, GPIO36, LPWR},
    rtc_cntl::{
        Rtc,
        sleep::{Ext0WakeupSource, Ext1WakeupSource, WakeupLevel},
    },
};
use log::info;
use meshtastenstein_core::ports::Sleep;

pub struct DeepSleepAdapter<'a> {
    rtc: Rtc<'a>,
}

impl<'a> DeepSleepAdapter<'a> {
    pub fn new(rtc_cntl: LPWR<'a>) -> Self {
        info!("[Sleep] Initializing deep sleep adapter");
        let rtc = Rtc::new(rtc_cntl);
        Self { rtc }
    }
}

impl<'a> Sleep for DeepSleepAdapter<'a> {
    fn enter_sleep(&mut self) -> ! {
        info!("[Sleep] ENTERING DEEP SLEEP");
        Delay::new().delay_millis(100u32);

        // SAFETY: We steal GPIO pins here immediately before entering deep sleep.
        // This is sound because:
        // 1. `enter_sleep` is diverging (`-> !`); the device will not return to user code
        //    after `sleep_deep()`, so there is no risk of aliased mutable pin access.
        // 2. All previously constructed GPIO handles are about to become irrelevant as
        //    the CPU is powered down; no other code runs concurrently at this point.
        unsafe {
            // VEXT (active low, per upstream's variant.h) powers the OLED
            // display and the LoRa antenna boost — NOT the SX1262 itself.
            // Drive it HIGH (off) before sleeping, matching upstream's
            // `digitalWrite(VEXT_ENABLE, !VEXT_ON_VALUE)`.
            let vext_pin = GPIO36::steal();
            let mut vext = Output::new(vext_pin, Level::High, OutputConfig::default());
            vext.set_high();

            // Hold LORA_CS (GPIO8) high across deep sleep: upstream's
            // `enableLoraInterrupt()` explicitly requires this ("LoRa CS
            // (RADIO_NSS) needs to stay HIGH, even during deep sleep") since
            // non-held GPIOs float when the CPU powers down, and a floating
            // CS can let the SX1262 misread bus noise as a real transaction.
            let mut cs_pin = GPIO8::steal();
            let mut cs = Output::new(cs_pin.reborrow(), Level::High, OutputConfig::default());
            cs.set_high();
            cs_pin.rtcio_pad_hold(true);

            // EXT0: LoRa DIO1 (GPIO 14) - wake on HIGH (incoming LoRa packet)
            let lora_dio = GPIO14::steal();
            let ext0 = Ext0WakeupSource::new(lora_dio, WakeupLevel::High);

            // EXT1: Button (GPIO 0) - wake on LOW (user button press)
            let mut wake_button = GPIO0::steal();
            let ext1_pins: &mut [&mut dyn esp_hal::gpio::RtcPin] = &mut [&mut wake_button];
            let ext1 = Ext1WakeupSource::new(ext1_pins, WakeupLevel::Low);

            self.rtc.sleep_deep(&[&ext0, &ext1]);
        }
    }
}
