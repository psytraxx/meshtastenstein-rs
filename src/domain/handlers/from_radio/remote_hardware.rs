use crate::domain::context::MeshCtx;
use crate::ports::MeshStorage;
use crate::proto::HardwareMessage;
use log::{info, warn};
use prost::Message;

pub async fn handle<S: MeshStorage>(_ctx: &mut MeshCtx<'_, S>, pkt: &super::InboundPacket<'_>) {
    let hw_msg = match HardwareMessage::decode(pkt.payload) {
        Ok(m) => m,
        Err(e) => {
            warn!(
                "[PortHandler] RemoteHardware decode failed from {:08x}: {:?}",
                pkt.sender, e
            );
            return;
        }
    };

    info!(
        "[PortHandler] RemoteHardware from {:08x}: type={} gpio_mask={:#010x} gpio_value={:#010x}",
        pkt.sender, hw_msg.r#type, hw_msg.gpio_mask, hw_msg.gpio_value,
    );
    // GPIO execution not implemented; packet is forwarded to BLE by central dispatch
    // so the phone app can render the hardware control UI.
}
