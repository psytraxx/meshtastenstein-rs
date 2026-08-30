//==============================================================================
// GPIO Pin Configuration (Heltec WiFi LoRa V3)
//==============================================================================

// `main.rs` wires GPIOs through esp-hal's typed peripheral singletons
// (`peripherals.GPIO8`), which can't be built from a `u8`, so most of these are
// documentation of the board's pinout rather than values the code reads. Only
// LORA_SS/LORA_BUSY are used, by the `AnyPin::steal()` calls in `lora_task`.
#[allow(dead_code, reason = "board pinout reference; see comment above")]
pub mod heltec_wifi_lora_v3 {
    /// LoRa SPI SCK pin
    pub const LORA_SCK: u8 = 9;
    /// LoRa SPI MISO pin
    pub const LORA_MISO: u8 = 11;
    /// LoRa SPI MOSI pin
    pub const LORA_MOSI: u8 = 10;
    /// LoRa SPI CS (chip select) pin
    pub const LORA_SS: u8 = 8;
    /// LoRa reset pin
    pub const LORA_RST: u8 = 12;
    /// LoRa DIO1 interrupt pin
    pub const LORA_DIO1: u8 = 14;
    /// LoRa BUSY pin
    pub const LORA_BUSY: u8 = 13;
    /// LED pin (active HIGH)
    pub const LED_PIN: u8 = 35;
    /// Wake button pin (active LOW with pull-up)
    pub const WAKE_BUTTON: u8 = 0;
    /// VEXT control pin
    pub const VEXT_PIN: u8 = 36;
    /// Battery voltage ADC pin
    pub const BATTERY_ADC_PIN: u8 = 1;
    /// Battery ADC control pin
    pub const BATTERY_ADC_CTRL: u8 = 37;
    /// Battery voltage divider ratio (Heltec V3: ~390K upper + 100K lower → ratio ≈ 4.9 × 1.045 trim = 5.1205)
    /// Matches official Meshtastic firmware ADC_MULTIPLIER for this board.
    pub const BATTERY_VOLTAGE_DIVIDER: f32 = 4.9 * 1.045;
}
