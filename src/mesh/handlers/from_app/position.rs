//! Handler for PortNum::PositionApp packets originating from the BLE app.

use super::{AppAction, AppContext};

/// M6: Position payloads from the app are saved for periodic re-broadcast,
/// then forwarded to LoRa as normal.
pub fn handle(_ctx: &AppContext<'_>) -> AppAction {
    AppAction::SavePositionAndTransmit
}
