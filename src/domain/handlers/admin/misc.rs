//! Handlers for simple AdminMessage variants:
//! BeginEditSettings, CommitEditSettings, FactoryResetConfig, RebootSeconds

use super::AdminResult;
use log::info;

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
    info!("[Admin] Factory reset — erasing config and rebooting");
    AdminResult {
        factory_reset: true,
        reboot_secs: Some(1),
        ..AdminResult::default()
    }
}

pub fn handle_nodedb_reset() -> AdminResult {
    info!("[Admin] NodeDB reset requested");
    AdminResult {
        nodedb_reset: true,
        ..AdminResult::default()
    }
}

pub fn handle_shutdown(secs: i32) -> AdminResult {
    if secs < 0 {
        info!("[Admin] Shutdown cancelled");
        AdminResult::default()
    } else {
        let delay = (secs as u64).max(1);
        info!("[Admin] Shutdown in {}s (deep sleep)...", delay);
        AdminResult {
            reboot_secs: Some(delay),
            ..AdminResult::default()
        }
    }
}

pub fn handle_remove_node(node_num: u32) -> AdminResult {
    info!("[Admin] Remove node {:08x} from DB", node_num);
    AdminResult {
        remove_nodenum: Some(node_num),
        ..AdminResult::default()
    }
}
