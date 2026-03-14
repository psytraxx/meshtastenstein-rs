//! Deep sleep adapter with GPIO and timer wakeup

use crate::ports::Sleep;
use esp_hal::{
    delay::Delay,
    gpio::{Level, Output, OutputConfig},
    peripherals::{GPIO0, GPIO14, GPIO36, LPWR},
    rtc_cntl::{
        Rtc,
        sleep::{Ext0WakeupSource, Ext1WakeupSource, WakeupLevel},
    },
};
use log::info;

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

        unsafe {
            // Disable VEXT
            let vext_pin = GPIO36::steal();
            let mut vext = Output::new(vext_pin, Level::Low, OutputConfig::default());
            vext.set_low();

            // EXT0: LoRa DIO1 (GPIO 14) - wake on HIGH
            let lora_dio = GPIO14::steal();
            let ext0 = Ext0WakeupSource::new(lora_dio, WakeupLevel::High);

            // EXT1: Button (GPIO 0) - wake on LOW
            let mut wake_button = GPIO0::steal();
            let ext1_pins: &mut [&mut dyn esp_hal::gpio::RtcPin] = &mut [&mut wake_button];
            let ext1 = Ext1WakeupSource::new(ext1_pins, WakeupLevel::Low);

            self.rtc.sleep_deep(&[&ext0, &ext1]);
        }
    }
}
