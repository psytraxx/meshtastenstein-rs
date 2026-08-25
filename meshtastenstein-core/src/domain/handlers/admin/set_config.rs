use crate::{
    domain::{
        context::MeshCtx,
        device::DeviceRole,
        radio_config::{ModemPreset, Region},
    },
    ports::MeshStorage,
    proto::{Config, config},
};
use log::info;

pub async fn handle<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, cfg: Config) {
    if let Some(variant) = cfg.payload_variant {
        match variant {
            config::PayloadVariant::Device(d) => {
                if let Ok(role) = DeviceRole::try_from(d.role) {
                    info!("[Admin] Setting device role to {:?}", role);
                    ctx.device.role = role;
                }
            }
            config::PayloadVariant::Lora(l) => {
                let region = Region::from_proto(l.region as u8);
                info!("[Admin] Setting region to {:?}", region);
                ctx.device.region = region as u8;

                if l.use_preset {
                    let preset = ModemPreset::from_proto(l.modem_preset as u8);
                    info!("[Admin] Setting modem preset to {:?}", preset);
                    ctx.device.modem_preset = preset;
                }
            }
            _ => {
                info!("[Admin] SetConfig for other variants (ignored)");
            }
        }
        ctx.storage.save_state(ctx.device);
    }
}
