//! Direct SX1262 SPI commands for reading buffered packets on wake
//! and setting sync word registers.
//!
//! Runs BEFORE lora-phy initialization to preserve buffered packets.

use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use embassy_time::Timer;
use embedded_hal::digital::OutputPin;
use embedded_hal_async::{digital::Wait, spi::SpiBus};
use log::{info, warn};

/// SX1262 SPI opcodes
const OPCODE_GET_STATUS: u8 = 0xC0;
const OPCODE_GET_RX_BUFFER_STATUS: u8 = 0x13;
const OPCODE_READ_BUFFER: u8 = 0x1E;
const OPCODE_GET_PACKET_STATUS: u8 = 0x14;
const OPCODE_GET_IRQ_STATUS: u8 = 0x12;
const OPCODE_WRITE_REGISTER: u8 = 0x0D;

/// SX1262 sync word registers
const REG_SYNC_WORD_MSB: u16 = 0x0740;

#[derive(Debug)]
pub enum Sx1262Error {
    Spi,
    Busy,
}

/// Write the Meshtastic sync word to SX1262 registers.
/// Must be called after lora-phy init but before first RX/TX.
///
/// For SX126x, the LoRa sync word register at 0x0740-0x0741:
///   MSB = (sync_word & 0xF0) | 0x04
///   LSB = ((sync_word & 0x0F) << 4) | 0x04
pub async fn write_sync_word<SPI, CS, BUSY>(
    spi_bus: &Mutex<CriticalSectionRawMutex, SPI>,
    cs: &mut CS,
    busy: &mut BUSY,
    sync_word_msb: u8,
    sync_word_lsb: u8,
) -> Result<(), Sx1262Error>
where
    SPI: SpiBus,
    CS: OutputPin,
    BUSY: Wait,
{
    busy.wait_for_low().await.map_err(|_| Sx1262Error::Busy)?;

    // WriteRegister command: [opcode, addr_msb, addr_lsb, data...]
    let mut cmd = [
        OPCODE_WRITE_REGISTER,
        (REG_SYNC_WORD_MSB >> 8) as u8,
        (REG_SYNC_WORD_MSB & 0xFF) as u8,
        sync_word_msb,
        sync_word_lsb,
    ];

    {
        let mut spi = spi_bus.lock().await;
        cs.set_low().map_err(|_| Sx1262Error::Spi)?;
        spi.transfer_in_place(&mut cmd)
            .await
            .map_err(|_| Sx1262Error::Spi)?;
        cs.set_high().map_err(|_| Sx1262Error::Spi)?;
    }

    info!(
        "[SX1262-Direct] Sync word set: MSB=0x{:02X}, LSB=0x{:02X}",
        sync_word_msb, sync_word_lsb
    );

    Ok(())
}

/// Read a packet from SX1262 buffer after wake from deep sleep.
pub async fn read_wake_packet<SPI, CS, BUSY>(
    spi_bus: &Mutex<CriticalSectionRawMutex, SPI>,
    cs: &mut CS,
    busy: &mut BUSY,
    buffer: &mut [u8],
) -> Result<Option<(u8, i16, i8)>, Sx1262Error>
where
    SPI: SpiBus,
    CS: OutputPin,
    BUSY: Wait,
{
    busy.wait_for_low().await.map_err(|_| Sx1262Error::Busy)?;
    Timer::after_micros(100).await;

    // GetStatus to wake SPI
    let mut status_buf = [OPCODE_GET_STATUS, 0x00];
    {
        let mut spi = spi_bus.lock().await;
        cs.set_low().map_err(|_| Sx1262Error::Spi)?;
        spi.transfer_in_place(&mut status_buf)
            .await
            .map_err(|_| Sx1262Error::Spi)?;
        cs.set_high().map_err(|_| Sx1262Error::Spi)?;
    }

    let chip_status = status_buf[1];
    let chip_mode = (chip_status >> 4) & 0x07;
    info!(
        "[SX1262-Direct] Chip status: 0x{:02X} (mode={})",
        chip_status, chip_mode
    );

    // Wait for RX completion if still receiving
    if chip_mode == 5 {
        info!("[SX1262-Direct] Chip in RX mode, waiting...");
        let mut rx_done = false;
        for i in 0..400 {
            Timer::after_millis(10).await;

            let mut irq_buf = [OPCODE_GET_IRQ_STATUS, 0x00, 0x00, 0x00];
            busy.wait_for_low().await.map_err(|_| Sx1262Error::Busy)?;
            {
                let mut spi = spi_bus.lock().await;
                cs.set_low().map_err(|_| Sx1262Error::Spi)?;
                spi.transfer_in_place(&mut irq_buf)
                    .await
                    .map_err(|_| Sx1262Error::Spi)?;
                cs.set_high().map_err(|_| Sx1262Error::Spi)?;
            }

            if irq_buf[3] & 0x02 != 0 {
                info!("[SX1262-Direct] RxDone after {}ms", (i + 1) * 10);
                rx_done = true;
                break;
            }
            if irq_buf[2] & 0x02 != 0 {
                warn!("[SX1262-Direct] RX Timeout");
                return Ok(None);
            }
        }
        if !rx_done {
            warn!("[SX1262-Direct] Timed out waiting for RxDone");
        }
    }

    busy.wait_for_low().await.map_err(|_| Sx1262Error::Busy)?;

    // GetRxBufferStatus
    let mut rx_status_buf = [OPCODE_GET_RX_BUFFER_STATUS, 0x00, 0x00, 0x00];
    {
        let mut spi = spi_bus.lock().await;
        cs.set_low().map_err(|_| Sx1262Error::Spi)?;
        spi.transfer_in_place(&mut rx_status_buf)
            .await
            .map_err(|_| Sx1262Error::Spi)?;
        cs.set_high().map_err(|_| Sx1262Error::Spi)?;
    }

    let payload_len = rx_status_buf[2];
    let buffer_offset = rx_status_buf[3];

    if payload_len == 0 {
        return Ok(None);
    }
    if payload_len as usize > buffer.len() {
        warn!("[SX1262-Direct] Packet too large: {}", payload_len);
        return Ok(None);
    }

    // ReadBuffer
    busy.wait_for_low().await.map_err(|_| Sx1262Error::Busy)?;
    {
        let mut spi = spi_bus.lock().await;
        cs.set_low().map_err(|_| Sx1262Error::Spi)?;
        let mut header = [OPCODE_READ_BUFFER, buffer_offset, 0x00];
        spi.transfer_in_place(&mut header)
            .await
            .map_err(|_| Sx1262Error::Spi)?;
        spi.read(&mut buffer[..payload_len as usize])
            .await
            .map_err(|_| Sx1262Error::Spi)?;
        cs.set_high().map_err(|_| Sx1262Error::Spi)?;
    }

    // GetPacketStatus for RSSI/SNR
    let mut pkt_status_buf = [OPCODE_GET_PACKET_STATUS, 0x00, 0x00, 0x00, 0x00];
    busy.wait_for_low().await.map_err(|_| Sx1262Error::Busy)?;
    {
        let mut spi = spi_bus.lock().await;
        cs.set_low().map_err(|_| Sx1262Error::Spi)?;
        spi.transfer_in_place(&mut pkt_status_buf)
            .await
            .map_err(|_| Sx1262Error::Spi)?;
        cs.set_high().map_err(|_| Sx1262Error::Spi)?;
    }

    let rssi: i16 = -(pkt_status_buf[2] as i16) / 2;
    let snr: i8 = (pkt_status_buf[3] as i8) / 4;

    info!(
        "[SX1262-Direct] Read {} bytes, RSSI={} dBm, SNR={} dB",
        payload_len, rssi, snr
    );

    Ok(Some((payload_len, rssi, snr)))
}
