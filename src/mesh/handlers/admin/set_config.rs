//! Handler for AdminMessage::SetConfig

use super::{AdminContext, AdminResult};
use crate::mesh::device::DeviceRole;
use crate::mesh::radio_config::ModemPreset;
use crate::proto::{Config, config};
use log::{debug, info};

pub fn handle(ctx: &mut AdminContext<'_>, cfg: Config) -> AdminResult {
    match cfg.payload_variant {
        Some(config::PayloadVariant::Lora(lora)) => {
            ctx.device.region = lora.region as u8;
            ctx.device.use_preset = lora.use_preset;
            ctx.device.channel_num = lora.channel_num;
            if lora.use_preset {
                ctx.device.modem_preset = ModemPreset::from_proto(lora.modem_preset as u8);
                info!(
                    "[Admin] LoRa config updated: region={} preset={} channel_num={}",
                    lora.region, lora.modem_preset, lora.channel_num
                );
            } else {
                // Custom SF/BW/CR (use_preset=false)
                let bw_hz = lora.bandwidth * 1000;
                ctx.device.custom_sf = lora.spread_factor as u8;
                ctx.device.custom_bw_hz = bw_hz;
                ctx.device.custom_cr = lora.coding_rate as u8;
                info!(
                    "[Admin] LoRa config updated: region={} custom SF={} BW={}kHz CR=4/{} channel_num={}",
                    lora.region,
                    lora.spread_factor,
                    lora.bandwidth,
                    lora.coding_rate,
                    lora.channel_num
                );
            }
        }
        Some(config::PayloadVariant::Device(dev)) => {
            ctx.device.role = match dev.role {
                0 => DeviceRole::Client,
                1 => DeviceRole::ClientMute,
                2 => DeviceRole::Router,
                3 => DeviceRole::RouterClient,
                4 => DeviceRole::Repeater,
                5 => DeviceRole::Tracker,
                6 => DeviceRole::Sensor,
                7 => DeviceRole::Tak,
                8 => DeviceRole::ClientHidden,
                9 => DeviceRole::LostAndFound,
                10 => DeviceRole::TakTracker,
                _ => DeviceRole::default(),
            };
            info!("[Admin] Device config updated: role={}", dev.role);
        }
        _ => {
            debug!("[Admin] SetConfig: unhandled config type");
        }
    }
    AdminResult {
        needs_persist: true,
        ..AdminResult::default()
    }
}
