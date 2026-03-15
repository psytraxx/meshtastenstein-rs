# CLAUDE.md — Meshtastenstein Project Guide

This file is for AI assistants working on this codebase. Read it at the start of every session.

---

## Project in One Sentence

`no_std` Rust implementation of the Meshtastic mesh protocol for Heltec WiFi LoRa 32 V3 (ESP32-S3 + SX1262), using Embassy async tasks and the trouble-host BLE stack.

---

## Toolchain & Build

- **Toolchain**: `esp` channel (Xtensa ESP Rust), managed by `rust-toolchain.toml`
- **Check**: `cargo check` — fast, no linker required, runs on the dev machine
- **Build/flash**: requires the Xtensa linker on target device; not available on this dev machine
- **Zero-warning policy**: always run `cargo check` after any change; fix all warnings before declaring done
- **Clippy**: `cargo clippy` also runs clean; `#![deny(clippy::mem_forget)]` and `#![deny(clippy::large_stack_frames)]` are enforced

### Protobuf

- Protobufs: `proto/meshtastic-protobufs/` (git submodule), generated to `src/proto/`
- Do NOT hand-edit `src/proto/*.rs` — regenerate with `cargo build` if protos change
- All proto types imported via `use crate::proto::{...}`

---

## Architecture Map

```
src/bin/main.rs          — peripheral init, NVS init (MUST be before LoRa spawn), task spawning
src/constants.rs         — ALL numeric constants (frequencies, timings, sizes, crypto)
src/mesh/radio_config.rs — Region + ModemPreset enums; frequency_hz(), from_proto(), default_channel_index()
src/mesh/device.rs       — DeviceState struct (node num, names, modem_preset, region, channels, role)
src/tasks/mesh_task.rs   — MeshOrchestrator: main loop (select4), admin handler, config exchange
src/tasks/lora_task.rs   — SX1262 init, TX queue, continuous RX, CAD jitter
src/tasks/ble_task.rs    — GATT server, pairing, from_radio_buf delivery, bond
src/adapters/nvs_storage_adapter.rs — Flash layout, SavedConfig, Bond, message ring
src/inter_task/channels.rs          — All Embassy Channel/Signal definitions
```

### Task spawning order (main.rs)

1. NVS init (`NvsStorageAdapter::new`) — MUST be first; loads preset/region for LoRa task
2. BLE bond load (`storage.load_bond()`)
3. LoRa preset/frequency computation from NVS (`Region::from_proto` + `ModemPreset::from_proto`)
4. Spawn: `lora_task` (with `preset` + `frequency_hz` params)
5. Spawn: `led_task`
6. Spawn: `battery_task`
7. Spawn: `ble_task` (needs `initial_bond`)
8. Spawn: `watchdog_task`
9. `MeshOrchestrator::run().await` — runs on main task (never returns)

---

## Key Invariants

### BLE packet delivery (ble_task.rs)
- `from_radio_buf: [u8; 512]` + `from_radio_len: usize` hold the current unread packet
- `from_radio_has_data: bool` gates the `tx_fut` in the `select` loop — **never overwrite an unread packet**
- Reads use `into_payload().reply(AttRsp::Read { data: &from_radio_buf[..from_radio_len] })` — exact byte length, no zero padding (Android MTU=508 → 512-byte response would be truncated → trailing-zero protobuf parse errors)
- Notifications: write `from_num` characteristic with the `from_radio_id` (u32 LE), THEN notify — phone reads `from_radio` in response to the notification

### LoRa radio parameters
- Sync word 0x2B MUST be written to SX1262 registers 0x0740/0x0741 (values 0x24/0xB4) after lora-phy init via `sx1262_direct::write_sync_word()`
- GPIO pins are `AnyPin::steal()`-ed for the direct register write; this is safe because it happens before the SPI bus is handed to lora-phy — see SAFETY comments in lora_task.rs
- Frequency is computed at boot: `preset.frequency_hz(region, region.default_channel_index())`; changing region/preset requires `RebootSeconds` + reboot because lora-phy doesn't support runtime reconfiguration

### Config exchange (mesh_task.rs `send_config_exchange`)
Full sequence required by Android app state machine (any missing message → app stays "connecting"):
1. `MyNodeInfo` (with `nodedb_count = 1 + node_db.len()`, `min_app_version: 20300`)
2. Own `NodeInfo` (FromRadio `node_info` variant)
3. `DeviceMetadata` (firmware_version, has_bluetooth, etc.)
4. 8× `Channel` (indices 0–7, Disabled if unconfigured)
5. 9× `Config` types: Device, Position, Power, Network, Display, LoRa, Bluetooth, Security, Sessionkey
6. 13× `ModuleConfig` types: Mqtt, Serial, ExternalNotification, StoreForward, RangeTest, Telemetry, CannedMessage, Audio, RemoteHardware, NeighborInfo, AmbientLighting, DetectionSensor, Paxcounter
7. NodeDB entries (one `FromRadio { node_info }` per stored node)
8. `ConfigCompleteId` (echoes the `want_config_id` from the phone's ToRadio)

### Admin message handling (mesh_task.rs)
- All admin messages arrive from BLE as `ADMIN_APP` (portnum 67) addressed to our node num
- `handle_admin_from_ble()` decodes, handles, and sends admin response back to BLE
- `SetConfig(LoRa)` → saves `region` + `modem_preset` to device state + NVS
- `SetConfig(Device)` → saves `role` to device state + NVS
- `RebootSeconds(n)` → `Timer::after(n secs).await` then `esp_hal::system::software_reset()` (diverges)
- Session passkey: derived from node_num XOR timestamp fragments; must be echoed in all admin responses
- `persist_config()` serializes `DeviceState` → `SavedConfig` → NVS flash

### NVS flash layout (within NVS partition)
```
0x0000–0x01FF  SavedConfig (512 bytes, magic=0x4D434647 "MCFG", version=1)
0x0200–0x022F  BLE Bond (48 bytes, magic=0x424F4E44 "BOND", version=1)
0x1000+        Message ring buffer (header at 0x1000, slots follow)
```

---

## Common Pitfalls

1. **`SetConfig` must save BOTH `region` AND `modem_preset`** — the app sends the full LoRa config struct even when only changing the preset. If you only save one field, the other gets corrupted on next config exchange.

2. **`RebootSeconds` must actually reboot** — the Meshtastic app always sends `RebootSeconds(N)` after any config change and waits for a reconnect. If the device doesn't reboot, Android sees a GATT_CONN_TIMEOUT (status=8) after ~30 s.

3. **MTU gotcha** — Android negotiates MTU=508 but we declare 512-byte GATT attributes. Replying with `accept()` returns all 512 bytes → phone receives 507 bytes (MTU-1) with trailing zeros → protobuf parse failure on every packet. Always use `AttRsp::Read { data: &buf[..len] }` for exact-length replies.

4. **BLE select() race** — in `ble_task.rs`, the `select` loop must gate `tx_fut` on `!from_radio_has_data`. Without this, the next packet can be pulled from the channel and overwrite `from_radio_buf` before the phone has read the current packet — silently dropping config exchange packets (app shows "no device selected").

5. **NVS init before LoRa spawn** — `main.rs` must initialize `NvsStorageAdapter` before spawning `lora_task` so the saved preset and region can be passed as parameters. The LoRa task can't be reconfigured at runtime.

6. **Default region** — `Region::default()` is `EU433` (code 2); `ModemPreset::default()` is `LongFast` (code 0). The default frequency for EU_433 / LongFast is 433.875 MHz (slot 3).

7. **`esp_hal::system::software_reset()`** — NOT `esp_hal::reset::software_reset()`. The module is `system`, not `reset`.

8. **Stack size** — `#![deny(clippy::large_stack_frames)]` is enforced. Large stack-allocated buffers inside async functions bloat the task state machine. Use `heapless::Vec` or heap allocation instead of large arrays inside async fns.

9. **`want_ack` flow** — if a packet addressed to us has `want_ack` set, we must send a routing ACK (`send_routing_ack`). If we send a packet with `want_ack`, track it in `pending_acks` for retransmission.

---

## Debugging Tips

- **Serial log level**: set `RUST_LOG=debug` env var before build; `esp_println` reads it at boot
- **Android adb logcat**: `adb logcat -s BluetoothGatt geeksville.mesh` — shows MTU negotiation, connection state, GATT reads/writes
- **Status codes**: Android `onClientConnectionState` status=8 = GATT_CONN_TIMEOUT (device vanished), status=22 = peer terminated, status=0 = success
- **Protobuf decode failures**: if BLE FromRadio payloads are malformed on the phone, check `from_radio_len` — should never be 0 or exceed actual encoded length
- **Frequency verify**: log line `[LoRa] Entering continuous RX mode at X Hz` — cross-check with expected formula: `region.start_hz + bw/2 + ch * bw`

---

## Proto Types Reference

Key imports pattern used throughout mesh_task.rs:
```rust
use crate::proto::{
    AdminMessage, Channel, ChannelSettings, Config, Data, DeviceMetadata, DeviceMetrics,
    FromRadio, MeshPacket, ModuleConfig, MyNodeInfo, NodeInfo as ProtoNodeInfo, PortNum,
    Telemetry, ToRadio, User,
    admin_message, config, from_radio, mesh_packet, module_config, telemetry, to_radio,
};
```

Key portnum constants: `PortNum::TextMessageApp`, `PortNum::NodeinfoApp`, `PortNum::PositionApp`, `PortNum::RoutingApp`, `PortNum::AdminApp`, `PortNum::TelemetryApp`

---

## What's Left (as of 2026-03-15)

- **M5**: Rebroadcast delay uses naive jitter; proper CSMA/CA not implemented
- **No unit tests**: priority candidates are packet encode/decode roundtrip, crypto nonce, duplicate detection, channel hash
- **Multi-preset at runtime**: frequency change requires reboot (by design); this is acceptable and matches official firmware behavior
- **Factory reset**: `FactoryResetConfig` admin handler is a stub (logs warning, does nothing)
- **FileManifest**: sent as empty in config exchange; fine for now
- **Waypoint forwarding**: portnum_handler logs but doesn't forward waypoints to BLE (comment says "todo")
