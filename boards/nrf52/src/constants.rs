//==============================================================================
// GPIO Pin Configuration (Seeed XIAO nRF52840 + Wio-SX1262 for XIAO)
//==============================================================================

// Pin assignments are cross-checked against two independent sources:
//   - Meshtastic upstream, variants/nrf52840/seeed_xiao_nrf52840_kit
//     (the SKU 102010710 / "Wio-SX1262 for XIAO V1.0" block)
//   - Seeed's own Wio-SX1262 header diagram, whose physical pin order
//     (D0, DIO1, RST, BUSY, NSS, RF_SW, D6) lands on XIAO pins D0..D6
//
// The XIAO's Arduino "Dxx" numbers are logical indices, not GPIO numbers; the
// nRF52840 port/pin each maps to is in the second column below.
//
// Note there are three different Wio-SX1262 pinouts in the wild (a legacy
// third-party SX126x layout and a 30-pin board-to-board variant). These are for
// the XIAO kit. If the radio doesn't respond, confirm which board you have
// before touching anything else.
//
// Unlike the Heltec board, this module has an RF switch: DIO2 drives the TX
// side internally, and RF_SW/RXEN must be asserted for RX.
#[allow(dead_code, reason = "board pinout reference; most are documentation")]
pub mod xiao_nrf52840_wio_sx1262 {
    /// LoRa SPI SCK — D8, P1.13
    pub const LORA_SCK: u8 = 45;
    /// LoRa SPI MISO — D9, P1.14
    pub const LORA_MISO: u8 = 46;
    /// LoRa SPI MOSI — D10, P1.15
    pub const LORA_MOSI: u8 = 47;
    /// LoRa SPI CS / NSS — D4, P0.04
    pub const LORA_SS: u8 = 4;
    /// LoRa reset — D2, P0.28
    pub const LORA_RST: u8 = 28;
    /// LoRa DIO1 interrupt — D1, P0.03
    pub const LORA_DIO1: u8 = 3;
    /// LoRa BUSY — D3, P0.29
    pub const LORA_BUSY: u8 = 29;
    /// RF switch RX enable — D5, P0.05. TX side is driven by the SX1262's own
    /// DIO2; this line must be high to receive.
    pub const LORA_RXEN: u8 = 5;

    /// TCXO supply voltage on DIO3, in volts (same as the Heltec board).
    pub const TCXO_VOLTAGE: f32 = 1.8;

    /// Green LED — P0.30. The RGB LED is common anode, so a LOW output lights it.
    pub const LED_GREEN: u8 = 30;
    /// Red LED — P0.26.
    pub const LED_RED: u8 = 26;
    /// Blue LED — P0.06.
    pub const LED_BLUE: u8 = 6;
    /// LEDs are active LOW (common anode).
    pub const LED_ACTIVE_LOW: bool = true;

    /// Battery voltage ADC — P0.31.
    pub const BATTERY_ADC_PIN: u8 = 31;
    /// Battery ADC divider enable — P0.14, sinks when driven LOW.
    pub const BATTERY_ADC_CTRL: u8 = 14;
    /// Battery voltage divider ratio (R17=1M, R18=510k).
    pub const BATTERY_VOLTAGE_DIVIDER: f32 = 3.0;
}
