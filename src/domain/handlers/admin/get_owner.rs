use crate::domain::context::MeshCtx;
use crate::domain::handlers::admin::send_admin_response;
use crate::ports::MeshStorage;
use crate::proto::{User, admin_message};
use log::debug;

pub async fn handle<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, requester: u32, req_pkt_id: u32) {
    debug!("[Admin] Handling GetOwnerRequest");

    let user = User {
        id: ctx.node_id_str.into(),
        long_name: ctx.device.long_name.as_str().into(),
        short_name: ctx.device.short_name.as_str().into(),
        hw_model: ctx.device.hw_model as i32,
        is_licensed: false,
        ..Default::default()
    };

    send_admin_response(
        ctx,
        requester,
        req_pkt_id,
        admin_message::PayloadVariant::GetOwnerResponse(user),
    )
    .await;
}
