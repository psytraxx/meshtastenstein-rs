use crate::{
    domain::{context::MeshCtx, handlers::admin::send_admin_response},
    ports::MeshStorage,
    proto::{ModuleConfig, admin_message, module_config},
};
use log::{debug, warn};

pub async fn handle<S: MeshStorage>(
    ctx: &mut MeshCtx<'_, S>,
    requester: u32,
    req_pkt_id: u32,
    config_type: i32,
) {
    debug!("[Admin] Handling GetModuleConfigRequest: {}", config_type);

    let Ok(config_type) = admin_message::ModuleConfigType::try_from(config_type) else {
        warn!("[Admin] Invalid module config_type: {}", config_type);
        return;
    };

    // All values are the same defaults sent during config exchange
    // (from_app::send_config_exchange); we don't yet track per-module settings
    // beyond what's already reflected there.
    let variant = match config_type {
        admin_message::ModuleConfigType::MqttConfig => {
            module_config::PayloadVariant::Mqtt(module_config::MqttConfig::default())
        }
        admin_message::ModuleConfigType::SerialConfig => {
            module_config::PayloadVariant::Serial(module_config::SerialConfig::default())
        }
        admin_message::ModuleConfigType::ExtnotifConfig => {
            module_config::PayloadVariant::ExternalNotification(
                module_config::ExternalNotificationConfig::default(),
            )
        }
        admin_message::ModuleConfigType::StoreforwardConfig => {
            module_config::PayloadVariant::StoreForward(module_config::StoreForwardConfig::default())
        }
        admin_message::ModuleConfigType::RangetestConfig => {
            module_config::PayloadVariant::RangeTest(module_config::RangeTestConfig::default())
        }
        admin_message::ModuleConfigType::TelemetryConfig => {
            module_config::PayloadVariant::Telemetry(module_config::TelemetryConfig::default())
        }
        admin_message::ModuleConfigType::CannedmsgConfig => {
            module_config::PayloadVariant::CannedMessage(
                module_config::CannedMessageConfig::default(),
            )
        }
        admin_message::ModuleConfigType::AudioConfig => {
            module_config::PayloadVariant::Audio(module_config::AudioConfig::default())
        }
        admin_message::ModuleConfigType::RemotehardwareConfig => {
            module_config::PayloadVariant::RemoteHardware(
                module_config::RemoteHardwareConfig::default(),
            )
        }
        admin_message::ModuleConfigType::NeighborinfoConfig => {
            module_config::PayloadVariant::NeighborInfo(module_config::NeighborInfoConfig::default())
        }
        admin_message::ModuleConfigType::AmbientlightingConfig => {
            module_config::PayloadVariant::AmbientLighting(
                module_config::AmbientLightingConfig::default(),
            )
        }
        admin_message::ModuleConfigType::DetectionsensorConfig => {
            module_config::PayloadVariant::DetectionSensor(
                module_config::DetectionSensorConfig::default(),
            )
        }
        admin_message::ModuleConfigType::PaxcounterConfig => {
            module_config::PayloadVariant::Paxcounter(module_config::PaxcounterConfig::default())
        }
        admin_message::ModuleConfigType::StatusmessageConfig => {
            module_config::PayloadVariant::Statusmessage(
                module_config::StatusMessageConfig::default(),
            )
        }
        admin_message::ModuleConfigType::TrafficmanagementConfig
        | admin_message::ModuleConfigType::TakConfig => {
            warn!(
                "[Admin] GetModuleConfigRequest for unimplemented module: {:?}",
                config_type
            );
            return;
        }
    };

    send_admin_response(
        ctx,
        requester,
        req_pkt_id,
        admin_message::PayloadVariant::GetModuleConfigResponse(ModuleConfig {
            payload_variant: Some(variant),
        }),
    )
    .await;
}
