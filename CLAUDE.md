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
- **Finishing policy**: always finish a task by running `cargo clippy` (fix any warnings) and `cargo fmt`

### Protobuf

- Protobufs: `proto/meshtastic-protobufs/` (git submodule), generated to `src/proto/`
- **`src/proto/meshtastic.rs` is gitignored** (generated file) — it exists on disk but won't appear in `git status`. Always treat it as present and up-to-date.
- Do NOT hand-edit `src/proto/*.rs` — regenerate with `cargo build` if protos change
- All proto types imported via `use crate::proto::{...}`

#### Proto types that share names with our domain types (naming collision, NOT actual duplication)
- `proto::DeviceState` — DB serialization type. **Never used**; our `domain::DeviceState` is the runtime config struct.
- `proto::ChannelSet` — URL-encoding type. **Never used**; our `domain::ChannelSet` is the runtime `[Option<ChannelConfig>; 8]` array.

#### Proto types that our domain enums duplicate (candidates for consolidation)
- `proto::channel::Role` (Disabled/Primary/Secondary) ↔ `domain::ChannelRole` — identical values; our version adds `try_from_proto(i32)`
- `proto::config::device_config::Role` (Client/Router/…) ↔ `domain::DeviceRole` — identical values; our version adds `TryFrom<u8>`

---

## Architecture Map

```
src/bin/main.rs                        — peripheral init, NVS init (MUST be before LoRa spawn), task spawning
src/constants.rs                       — ALL numeric constants (frequencies, timings, sizes, crypto)
src/inter_task/channels.rs             — All Embassy Channel/Signal definitions + MeshEvent enum

src/domain/
  context.rs                           — MeshCtx (passed by &mut to all handlers) + ChannelMetrics
  pending.rs                           — PendingPacket + PendingRebroadcast structs
  device.rs                            — DeviceState (node num, names, modem_preset, region, channels, role)
  node_db.rs                           — NodeDB + NodeEntry (known peers)
  router.rs                            — MeshRouter: duplicate detection, rebroadcast decision, FilterResult
  radio_config.rs                      — Region + ModemPreset enums; frequency_hz(), from_proto()
  crypto.rs                            — AES-128-CTR packet encryption/decryption
  packet.rs                            — RadioFrame, PacketHeader, HEADER_SIZE, BROADCAST_ADDR

  handlers/
    mod.rs                             — Top-level MeshEvent dispatcher → from_radio / from_app / periodic
    from_radio/mod.rs                  — LoRa RX: 3-layer pipeline (own-packet → filter → portnum dispatch)
    from_radio/{portnum}.rs            — Per-portnum handlers: node_info, position, routing, traceroute, …
    from_app/mod.rs                    — BLE RX: decode ToRadio, config exchange, transmit_from_ble_packet
    from_app/position.rs               — BLE position save (M6)
    admin/mod.rs                       — AdminMessage dispatch from LoRa (portnum 67) addressed to us
    admin/{action}.rs                  — get_config, set_config, get_owner, set_owner, set_channel, misc
    periodic.rs                        — Tick handler: NodeInfo, NeighborInfo, position, telemetry broadcasts
    util.rs                            — Shared helpers: forward_to_ble, lora_send, send_routing_ack, encode_from_radio
    outgoing/                          — Payload builders: node_info::build_payload, telemetry::build_payload

src/tasks/
  mesh_task.rs                         — MeshOrchestrator: event loop (select3), make_ctx(), retransmissions
  lora_task.rs                         — SX1262 init, TX queue, continuous RX, CAD jitter
  ble_task.rs                          — GATT server, pairing, from_radio_buf delivery, bond
  battery_task.rs                      — ADC battery level + voltage sensing
  led_task.rs                          — LED blink pattern executor
  watchdog_task.rs                     — Embassy watchdog feed

src/adapters/
  nvs_storage_adapter.rs               — Flash layout, SavedConfig, Bond, message ring buffer
  esp_identity_adapter.rs              — MAC-based node ID derivation
  deep_sleep_adapter.rs                — Deep sleep support

src/ports/                             — Trait definitions (MeshStorage, Identity, Sleep)
src/drivers/sx1262_direct.rs           — Direct SX1262 register access (sync word write)
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

## Event Flow

```
LoRa RX  →  lora_task  →  channels.mesh_in (MeshEvent::LoraRx)
BLE RX   →  ble_task   →  channels.mesh_in (MeshEvent::BleRx)
Battery  →  battery_task → channels.mesh_in (MeshEvent::BatteryUpdate)
…other signals           → channels.mesh_in (MeshEvent::BleConnected/Disconnected/BondSave/ChannelUtilUpdate)

MeshOrchestrator::next_event()
  select3(mesh_in.receive(), timers, heartbeat.next())
  → MeshEvent

handlers::dispatch(event, &mut ctx)
  LoraRx   → from_radio::dispatch  → portnum handlers → forward_to_ble | send_routing_ack | rebroadcast
  BleRx    → from_app::dispatch    → config exchange | transmit_from_ble_packet | admin::dispatch
  Tick     → periodic::dispatch    → NodeInfo | NeighborInfo | position | telemetry broadcasts
  Battery  → periodic::send_device_telemetry
  …
```

**Adding a new LoRa portnum handler:**
1. Create `src/domain/handlers/from_radio/my_portnum.rs` with `pub async fn handle(ctx, sender, payload)`
2. Add `pub mod my_portnum;` in `from_radio/mod.rs`
3. Add a match arm: `Some(PortNum::MyPortnum) => my_portnum::handle(ctx, ...).await`
4. Handler may call: `forward_to_ble`, `send_routing_ack`, update `ctx` state

**Adding a new BLE → LoRa feature:**
- Add a portnum arm in `from_app::transmit_from_ble_packet` (or handle locally and `return` early)

---

## Key Invariants

### MeshCtx — the context struct
`MeshCtx<'_, S>` is created fresh each event loop iteration via `make_ctx()` and passed by `&mut`
to all handlers. It is a projection of `MeshOrchestrator` fields (all refs, no owned data).

Key fields:
- `device: &mut DeviceState` — node config, channels, role, modem_preset
- `node_db: &mut NodeDB` — known peers
- `router: &mut MeshRouter` — duplicate detection + rebroadcast state
- `pending_packets: &mut Vec<PendingPacket, 8>` — want_ack retransmit queue
- `pending_rebroadcast: &mut Option<PendingRebroadcast>` — next scheduled flood relay
- `session_passkey: &mut Option<[u8; 16]>` — `None` until first admin message (lazy init)
- `channel_metrics: &mut ChannelMetrics` — `{ channel_util: f32, air_util_tx: f32 }`
- `reboot_after_secs: &mut Option<u32>` — set by `RebootSeconds` admin; orchestrator reboots after dispatch
- `tx_to_ble`, `tx_to_lora`, `led_commands` — Embassy `Sender` handles (Copy)

### LoRa RX pipeline — 3 layers in `from_radio::dispatch`
1. **Layer 0: Own-packet check** — if `header.sender == our node_num`, cancel pending ACK (implicit ACK) and drop
2. **Layer 1: FloodingRouter filter** — `router.should_filter_received()` returns `FilterResult`:
   - `New` → process normally
   - `DuplicateUpgrade(new_hop)` → upgrade pending rebroadcast, return
   - `DuplicateCancelRelay` → cancel our pending rebroadcast (another node relayed already), return
   - `DuplicateDrop` → drop, return
3. **Layer 2: Portnum dispatch** — per-portnum handler + default BLE forward + routing ACK
4. **Layer 3: Rebroadcast decision** — schedule `PendingRebroadcast` with jittered delay

### BLE packet delivery (ble_task.rs)
- `from_radio_buf: [u8; 512]` + `from_radio_len: usize` hold the current unread packet
- `from_radio_has_data: bool` gates the `tx_fut` in the `select` loop — **never overwrite an unread packet**
- Reads use `into_payload().reply(AttRsp::Read { data: &from_radio_buf[..from_radio_len] })` — exact byte length, no zero padding (Android MTU=508 → 512-byte response would be truncated → trailing-zero protobuf parse errors)
- Notifications: write `from_num` characteristic with the `from_radio_id` (u32 LE), THEN notify — phone reads `from_radio` in response to the notification

### Config exchange (`from_app::dispatch` → `send_config_exchange`)
Full sequence required by Android app state machine (any missing message → app stays "connecting"):
1. `MyNodeInfo` (with `nodedb_count = 1 + node_db.len()`, `min_app_version: 20300`)
2. Own `NodeInfo` (FromRadio `node_info` variant)
3. `DeviceMetadata` (firmware_version, has_bluetooth, etc.)
4. 8× `Channel` (indices 0–7, Disabled if unconfigured)
5. 9× `Config` types: Device, Position, Power, Network, Display, LoRa, Bluetooth, Security, Sessionkey
6. 13× `ModuleConfig` types: Mqtt, Serial, ExternalNotification, StoreForward, RangeTest, Telemetry, CannedMessage, Audio, RemoteHardware, NeighborInfo, AmbientLighting, DetectionSensor, Paxcounter
7. NodeDB entries (one `FromRadio { node_info }` per stored node)
8. `ConfigCompleteId` (echoes the `want_config_id` from the phone's ToRadio)

### Admin message handling (`handlers/admin/`)
- All admin messages arrive as `ADMIN_APP` (portnum 67) addressed to our node num
- `admin::dispatch(ctx, sender, packet_id, payload)` decodes and routes to sub-handlers
- `SetConfig(LoRa)` → saves `region` + `modem_preset` to device state + NVS
- `SetConfig(Device)` → saves `role` to device state + NVS
- `RebootSeconds(n)` → sets `ctx.reboot_after_secs = Some(n)`; orchestrator performs `esp_hal::system::software_reset()` after dispatch completes
- Session passkey: `ctx.session_passkey` is `None` on first boot; admin handlers lazy-init via `ensure_session_passkey(ctx)`; must be echoed in all admin responses
- `persist_config()` serializes `DeviceState` → `SavedConfig` → NVS flash

### LoRa radio parameters
- Sync word 0x2B MUST be written to SX1262 registers 0x0740/0x0741 (values 0x24/0xB4) after lora-phy init via `sx1262_direct::write_sync_word()`
- GPIO pins are `AnyPin::steal()`-ed for the direct register write; this is safe because it happens before the SPI bus is handed to lora-phy — see SAFETY comments in lora_task.rs
- Frequency is computed at boot: `preset.frequency_hz(region, region.default_channel_index())`; changing region/preset requires `RebootSeconds` + reboot because lora-phy doesn't support runtime reconfiguration

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

8. **Stack size** — `#![deny(clippy::large_stack_frames)]` is enforced. Large stack-allocated buffers inside async functions bloat the task state machine. Use `heapless::Vec` or heap allocation instead of large arrays inside async fns. `Box<RadioFrame>` and `Box<ToRadioMessage>` in `MeshEvent` are intentional for this reason.

9. **`want_ack` flow** — if a packet addressed to us has `want_ack` set, we must send a routing ACK (`send_routing_ack`). If we send a packet with `want_ack`, track it in `pending_packets` for retransmission. `PendingPacket` tracks `is_our_packet` and on last retry clears `next_hop` to fall back to flooding.

---

## Debugging Tips

- **Serial log level**: set `RUST_LOG=debug` env var before build; `esp_println` reads it at boot
- **Android adb logcat**: `adb logcat -s BluetoothGatt geeksville.mesh` — shows MTU negotiation, connection state, GATT reads/writes
- **Status codes**: Android `onClientConnectionState` status=8 = GATT_CONN_TIMEOUT (device vanished), status=22 = peer terminated, status=0 = success
- **Protobuf decode failures**: if BLE FromRadio payloads are malformed on the phone, check `from_radio_len` — should never be 0 or exceed actual encoded length
- **Frequency verify**: log line `[LoRa] Entering continuous RX mode at X Hz` — cross-check with expected formula: `region.start_hz + bw/2 + ch * bw`

---

## Proto Types Reference

Key imports used in handler modules:
```rust
use crate::proto::{
    AdminMessage, Channel, ChannelSettings, Config, Data, DeviceMetadata,
    FromRadio, MeshPacket, ModuleConfig, MyNodeInfo, NodeInfo as ProtoNodeInfo, PortNum,
    Routing, Telemetry, ToRadio, User,
    admin_message, config, from_radio, mesh_packet, module_config, routing, to_radio,
};
```

Key portnum constants: `PortNum::TextMessageApp`, `PortNum::NodeinfoApp`, `PortNum::PositionApp`, `PortNum::RoutingApp`, `PortNum::AdminApp`, `PortNum::TelemetryApp`, `PortNum::TracerouteApp`, `PortNum::NeighborinfoApp`

---

## What's Left (as of 2026-03-23)

- **Multi-preset at runtime**: frequency change requires reboot (by design); matches official firmware behavior
- **FileManifest**: sent empty in config exchange; fine for now
- **Hierarchical routing**: `next_hop` / `relay_node` fields in `PendingPacket` and `FilterResult` are wired but routing table learning (updating `NodeEntry::next_hop` from observed relays) is partial
