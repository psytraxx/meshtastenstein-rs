use crate::{
    domain::{
        context::MeshCtx, device::DeviceState, handlers::admin::send_admin_response,
        radio_config::bw_hz_to_code,
    },
    ports::MeshStorage,
    proto::{Config, admin_message, config},
};
use log::debug;

pub async fn handle<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    requester: u32,
    req_pkt_id: u32,
    config_type: admin_message::ConfigType,
    via_lora: bool,
) {
    debug!("[Admin] Handling GetConfigRequest: {:?}", config_type);

    let variant = match config_type {
        admin_message::ConfigType::DeviceConfig => {
            config::PayloadVariant::Device(config::DeviceConfig {
                role: ctx.device.role as i32,
                ..Default::default()
            })
        }
        admin_message::ConfigType::LoraConfig => {
            config::PayloadVariant::Lora(build_lora_config(ctx.device))
        }
        admin_message::ConfigType::BluetoothConfig => {
            config::PayloadVariant::Bluetooth(config::BluetoothConfig {
                enabled: true,
                mode: config::bluetooth_config::PairingMode::RandomPin as i32,
                ..Default::default()
            })
        }
        admin_message::ConfigType::PositionConfig => {
            config::PayloadVariant::Position(config::PositionConfig::default())
        }
        admin_message::ConfigType::PowerConfig => {
            config::PayloadVariant::Power(config::PowerConfig::default())
        }
        admin_message::ConfigType::NetworkConfig => {
            config::PayloadVariant::Network(config::NetworkConfig::default())
        }
        admin_message::ConfigType::DisplayConfig => {
            config::PayloadVariant::Display(config::DisplayConfig::default())
        }
        admin_message::ConfigType::SecurityConfig => {
            config::PayloadVariant::Security(config::SecurityConfig::default())
        }
        admin_message::ConfigType::SessionkeyConfig => {
            config::PayloadVariant::Sessionkey(config::SessionkeyConfig {})
        }
        admin_message::ConfigType::DeviceuiConfig => {
            // DeviceUiConfig is a top-level proto type, not nested under config::
            config::PayloadVariant::DeviceUi(crate::proto::DeviceUiConfig::default())
        }
    };

    send_admin_response(
        ctx,
        requester,
        req_pkt_id,
        admin_message::PayloadVariant::GetConfigResponse(Config {
            payload_variant: Some(variant),
        }),
        via_lora,
    )
    .await;
}

pub fn build_lora_config(device: &DeviceState) -> config::LoRaConfig {
    config::LoRaConfig {
        use_preset: device.use_preset,
        modem_preset: device.modem_preset as i32,
        region: device.region as i32,
        hop_limit: device.hop_limit as u32,
        tx_enabled: device.tx_enabled,
        config_ok_to_mqtt: device.config_ok_to_mqtt,
        spread_factor: device.custom_sf as u32,
        bandwidth: bw_hz_to_code(device.custom_bw_hz) as u32,
        coding_rate: device.custom_cr as u32,
        channel_num: device.channel_num,
        // Reported to the app rather than left at the proto's 0 default,
        // matching upstream's resolve-tx_power-before-reporting behavior
        // (RadioInterface::applyModemConfig writes the resolved power back
        // onto loraConfig.tx_power). This firmware always transmits at the
        // fixed LORA_TX_POWER_DBM constant, so the resolved value is constant too.
        tx_power: crate::constants::LORA_TX_POWER_DBM,
        ..Default::default()
    }
}
