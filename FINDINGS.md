# Meshtastenstein – Interoperability Findings

Audit against official Meshtastic firmware requirements.
Tracked here so nothing gets lost across sessions.

---

## FATAL (blocking phone app)

| # | Status | Gap | File |
|---|--------|-----|------|
| F1 | ~~done~~ | Config exchange incomplete — missing `Config { lora }` and `Channel` messages; also wrong `config_complete_id` field number (13 instead of 7) | `mesh_task.rs` |
| F2 | ~~done~~ | Admin messages stub — `ADMIN_APP` portnum now decoded in mesh_task; get/set owner, config, channel; session passkey derived from node_num | `mesh_task.rs` |
| F3 | ~~done~~ | Prost-generated types unused — manual codec only covers ~20% of fields, will silently corrupt messages with repeated fields / nested structs | `mesh_task.rs`, `proto/` |

---

## IMPORTANT (major missing features)

| # | Status | Gap | File |
|---|--------|-----|------|
| I1 | ~~done~~ | NodeInfo broadcast on boot (5s delay) + every 15 min; responds to `want_response` NodeInfo requests | `mesh_task.rs` |
| I2 | ~~done~~ | NVS persistence: `SavedConfig` written to flash sector 0 of NVS partition; loaded on boot; saved after SetOwner/SetConfig/SetChannel/CommitEditSettings | `nvs_storage_adapter.rs`, `mesh_task.rs` |
| I3 | ~~done~~ | `watchdog_task` now receives `&'static mut DeepSleepAdapter`; after inactivity timeout + 500ms BLE grace period calls `enter_sleep()` → DIO1/button wakeup | `watchdog_task.rs` |
| I4 | ~~done~~ | Store-and-forward: TEXT_MESSAGE (portnum 1) buffered in NVS when BLE disconnected; replayed to phone after config exchange | `mesh_task.rs` |
| I5 | ~~done~~ | Region hardcoded US only — EU_433 now default (433.625 MHz, ch2); `Region` enum added for all regions | `constants.rs`, `radio_config.rs` |
| I6 | ~~done~~ | Battery level sent as `TELEMETRY_APP` (portnum 67) `FromRadio` packet when bat_level Signal fires; `bat_level` changed to Signal so mesh_task observes it | `mesh_task.rs`, `battery_task.rs` |
| I7 | ~~done~~ | Channel config not sent in config exchange | `mesh_task.rs` |

---

## MODERATE (protocol compliance)

| # | Status | Gap | File |
|---|--------|-----|------|
| M1 | ~~done~~ | want_ack retransmission: `PendingAck` ring (8 slots), 3 retries × 5s timeout; ACK clears slot; timeout drops and warns | `mesh_task.rs` |
| M2 | ~~done~~ | Telemetry forwarded to BLE (all LoRa packets forwarded); own device telemetry sent on battery update | `mesh_task.rs` |
| M3 | ~~done~~ | Traceroute/NeighborInfo (70/71) now handled in portnum_handler (log + pass-through to BLE) | `portnum_handler.rs` |
| M4 | ~~done~~ | NodeDB synced to phone in config exchange (`FromRadio { node_info }`); updated on each NodeInfo/Position from LoRa | `mesh_task.rs` |
| M5 | todo | Rebroadcast delay oversimplified | `router.rs` |
| M6 | ~~done~~ | Position broadcast: phone's POSITION_APP payload saved; re-broadcast to mesh every 30 min | `mesh_task.rs` |

---

## MINOR

| # | Status | Gap |
|---|--------|-----|
| N1 | todo | `TEXT_MESSAGE_COMPRESSED` (portnum 7) not handled |
| N2 | todo | `WAYPOINT_APP` (portnum 8) not handled |
| N3 | todo | `REMOTE_HARDWARE_APP` (portnum 2) not handled |
| N4 | todo | `FromNum` semantics slightly off (should convey pending queue depth) |

---

## Stage log

| Stage | Items | Status |
|-------|-------|--------|
| Stage 1 | F1, I5, I7 — complete config exchange (LoRa config + channels), fix wrong field number, EU_433 region | ✅ done |
| Stage 2 | F3 — switch to prost types for reliable encode/decode | ✅ done |
| Stage 3 | F2 — admin messages (get/set config, session passkey) | ✅ done |
| Stage 4 | I1 — broadcast NodeInfo on boot + periodically | ✅ done |
| Stage 5 | I2 — NVS persistence for config + channels + node num | ✅ done |
| Stage 6 | I6, M2, M3, M4 — battery level char, telemetry, traceroute, node DB sync | ✅ done |
| Stage 7 | I3 — deep sleep trigger from watchdog | ✅ done |
| Stage 8 | I4, M1, M6 — store-forward, retransmission, position broadcast | ✅ done |
| Stage 9 | N1–N4 — minor portnum handlers, FromNum semantics | todo |
