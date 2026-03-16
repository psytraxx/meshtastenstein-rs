//! Handlers for simple AdminMessage variants:
//! BeginEditSettings, CommitEditSettings, FactoryResetConfig, RebootSeconds

use super::AdminResult;
use log::{info, warn};

pub fn handle_begin_edit() -> AdminResult {
    info!("[Admin] BeginEditSettings");
    AdminResult::default()
}

pub fn handle_commit_edit() -> AdminResult {
    info!("[Admin] CommitEditSettings — persisting config");
    AdminResult {
        needs_persist: true,
        ..AdminResult::default()
    }
}

pub fn handle_reboot(secs: i32) -> AdminResult {
    let delay = (secs as u64).max(1);
    info!("[Admin] Rebooting in {}s to apply config...", delay);
    AdminResult {
        reboot_secs: Some(delay),
        ..AdminResult::default()
    }
}

pub fn handle_factory_reset() -> AdminResult {
    warn!("[Admin] Factory reset requested (not implemented)");
    AdminResult::default()
}
