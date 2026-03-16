//! Handler for AdminMessage::GetConfigRequest
//!
//! Also exposes `build_lora_config` for use in the config exchange sequence.

use super::{AdminContext, AdminResult};
use crate::constants::{DEFAULT_HOP_LIMIT, LORA_TX_POWER_DBM};
use crate::domain::device::DeviceState;
use crate::proto::{Config, admin_message, config};
use log::info;

/// Build the LoRa config proto from device state.
/// Used both by GetConfigRequest and the config exchange sequence.
pub fn build_lora_config(device: &DeviceState) -> config::LoRaConfig {
    let region = device.region as i32;
    if device.use_preset {
        config::LoRaConfig {
            use_preset: true,
            modem_preset: device.modem_preset as i32,
            region,
            channel_num: device.channel_num,
            hop_limit: DEFAULT_HOP_LIMIT as u32,
            tx_enabled: true,
            tx_power: LORA_TX_POWER_DBM,
            ..Default::default()
        }
    } else {
        config::LoRaConfig {
            use_preset: false,
            region,
            bandwidth: device.custom_bw_hz / 1000,
            spread_factor: device.custom_sf as u32,
            coding_rate: device.custom_cr as u32,
            channel_num: device.channel_num,
            hop_limit: DEFAULT_HOP_LIMIT as u32,
            tx_enabled: true,
            tx_power: LORA_TX_POWER_DBM,
            ..Default::default()
        }
    }
}

pub fn handle(ctx: &AdminContext<'_>, config_type: i32) -> AdminResult {
    info!("[Admin] GetConfigRequest type={}", config_type);
    let cfg = match config_type {
        5 => Config {
            // LoRaConfig
            payload_variant: Some(config::PayloadVariant::Lora(build_lora_config(ctx.device))),
        },
        0 => Config {
            // DeviceConfig
            payload_variant: Some(config::PayloadVariant::Device(config::DeviceConfig {
                role: ctx.device.role as i32,
                ..Default::default()
            })),
        },
        1 => Config {
            // PositionConfig
            payload_variant: Some(config::PayloadVariant::Position(
                config::PositionConfig::default(),
            )),
        },
        2 => Config {
            // PowerConfig
            payload_variant: Some(config::PayloadVariant::Power(config::PowerConfig::default())),
        },
        3 => Config {
            // NetworkConfig
            payload_variant: Some(config::PayloadVariant::Network(
                config::NetworkConfig::default(),
            )),
        },
        4 => Config {
            // DisplayConfig
            payload_variant: Some(config::PayloadVariant::Display(
                config::DisplayConfig::default(),
            )),
        },
        6 => Config {
            // BluetoothConfig
            payload_variant: Some(config::PayloadVariant::Bluetooth(config::BluetoothConfig {
                enabled: true,
                mode: config::bluetooth_config::PairingMode::RandomPin as i32,
                ..Default::default()
            })),
        },
        7 => Config {
            // SecurityConfig
            payload_variant: Some(config::PayloadVariant::Security(
                config::SecurityConfig::default(),
            )),
        },
        _ => Config {
            payload_variant: None,
        },
    };
    AdminResult {
        response: Some(admin_message::PayloadVariant::GetConfigResponse(cfg)),
        ..AdminResult::default()
    }
}
