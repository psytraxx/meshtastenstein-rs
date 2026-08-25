# CLAUDE.md — Meshtastenstein Project Guide

This file is for AI assistants working on this codebase. Read it at the start of every session.

---

## Project in One Sentence

`no_std` Rust implementation of the Meshtastic mesh protocol, using Embassy async tasks and the trouble-host BLE stack. The protocol lives in a hardware-agnostic core crate; Heltec WiFi LoRa 32 V3 (ESP32-S3 + SX1262) is currently the only board, with XIAO nRF52840 + Wio-SX1262 planned.

---

## Crate layout

**Independent crates, NOT a Cargo workspace.** There is no top-level `Cargo.toml`
and no top-level `cargo build` — always `cd` into a crate first. The boards need
different compilers, which one `rust-toolchain.toml` and one lockfile can't express.

| Path | Contents | Toolchain |
| --- | --- | --- |
| `meshtastenstein-core/` | Protocol, routing, crypto, persistence, port traits, SX1262 driver | `stable` |
| `boards/esp32/` | Heltec WiFi LoRa V3: radio, BLE, flash, battery, watchdog + pinout | `esp` (Xtensa) |
| `boards/nrf52/` | Seeed XIAO nRF52840 + Wio-SX1262. **Bring-up in progress** | `stable` (thumbv7em) |

**The BLE stack lives in the board crates, not core.** The ESP32's `esp-radio`
controller and the nRF52's `nrf-sdc` need different `bt-hci` majors, and
`bt-hci` defines the `Controller` trait bridging host and controller — so the
boards pin different `trouble-host` versions (a 0.6 git rev vs. HEAD/0.8).
Core carries only the protocol-level BLE constants.

Each crate owns its `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`,
`.clippy.toml`, `rustfmt.toml` and CI job. `boards/esp32` also owns
`.cargo/config.toml` (target + flash runner). Board crates depend on core by
relative path (`meshtastenstein-core = { path = "../../meshtastenstein-core" }`).

Adding a board means a new `boards/<name>/` with that full set of files, plus a
CI job in `.github/workflows/rust_ci.yml`. Nothing in `meshtastenstein-core` may
gain a chip dependency — that's the whole point of the split.

## Toolchain & Build

- **Check**: `cd meshtastenstein-core && cargo check` (stable), `cd boards/esp32 && cargo check` (Xtensa). Both are fast and need no linker.
- **Build/flash**: requires the Xtensa linker on target device; not available on this dev machine
- **Zero-warning policy**: run `cargo check` in **both** crates after any change touching shared code; fix all warnings before declaring done
- **Clippy**: both crates run clean under `cargo clippy --all-features -- -D warnings` (what CI runs). `#![deny(clippy::mem_forget)]` and `#![deny(clippy::large_stack_frames)]` are enforced on the board crate. Note the two crates use **different clippy versions** (stable vs. the esp toolchain's), so core can surface lints the board crate doesn't — check core too.
- **Stack-frame threshold**: `boards/esp32/.clippy.toml` sets `stack-size-threshold = 32768`, vs. 1024 in core. The board's async task state machines are sized individually rather than collapsed, so they legitimately report ~24 KB. Don't "fix" this by lowering it without checking real task stack sizes.
- **Finishing policy**: always finish a task by running `cargo clippy` and `cargo fmt` **in each crate you touched**, updating `CHANGELOG.md` (add an entry under today's date), and keeping `README.md` consistent with the changes (features list, use-case table, NVS layout, Known Limitations, What's Left)

### CHANGELOG style — keep entries high level

Write for someone who wants to know *what changed and why*, not *how it was implemented*.

- **No filenames, module paths, function names, struct fields or constant names.** Say "the shared transmit builder", not `TxBuilder::build()` in `tx.rs`. Say "the duplicate-detection cache", not `DUPLICATE_RING_SIZE`.
- **No documentation-only changes.** README/CLAUDE.md edits, comment fixes and typo passes do not get entries. A code change that *came out of* a docs pass (e.g. deleting a dead constant) does get one, described as the code change it is.
- **One or two sentences per entry.** State the behaviour change and the reason. Drop line counts, internal refactor mechanics and upstream C++ symbol names — "matching upstream" is enough.
- **Use the standard sections only**: `Added`, `Changed`, `Fixed`, `Removed`. Don't invent new ones (`Diagnostics`, `Notes`, `Documentation`); fold those items into `Changed` or leave them out.
- **Group by date.** Every block is a plain `## YYYY-MM-DD` heading, newest at the top — there are no releases, so no `[Unreleased]`. Add to today's block if one exists, otherwise start a new one; never repeat a section heading within a block. (The two `[Phase N]` blocks at the bottom predate this and stay as they are.)

### Protobuf

- Protobufs: `proto/meshtastic-protobufs/` (git submodule at the repo root, shared), generated into `meshtastenstein-core/src/proto/` by that crate's `build.rs`
- **`meshtastenstein-core/src/proto/meshtastic.rs` is gitignored** (generated file) — it exists on disk but won't appear in `git status`. Always treat it as present and up-to-date.
- Do NOT hand-edit `meshtastenstein-core/src/proto/*.rs` — regenerate with `cd meshtastenstein-core && cargo build` if protos change
- Within core, proto types are imported via `use crate::proto::{...}`; from a board crate, `use meshtastenstein_core::proto::{...}`

#### Proto types that share names with our domain types (naming collision, NOT actual duplication)
- `proto::DeviceState` — DB serialization type. **Never used**; our `domain::DeviceState` is the runtime config struct.
- `proto::ChannelSet` — URL-encoding type. **Never used**; our `domain::ChannelSet` is the runtime `[Option<ChannelConfig>; 8]` array.

#### Domain "enums" that are really proto re-exports (no duplication — do not re-add one)
- `domain::ChannelRole` **is** `proto::channel::Role` — `pub use` re-export in `domain/channels.rs`
- `domain::DeviceRole` **is** `proto::config::device_config::Role` — `pub use` re-export in `domain/device.rs`
- `domain::radio_config::{Region, ModemPreset}` are re-exports of `proto::config::lo_ra_config::{RegionCode, ModemPreset}`

Convert from a wire value with prost's generated `TryFrom<i32>` (e.g.
`DeviceRole::try_from(d.role)`, `ChannelRole::try_from(ch.role)`). `Region`/`ModemPreset`
additionally have `from_proto(u8)` helpers that apply a default on an unknown value.
There is no `try_from_proto` anywhere in this codebase.

---

## Architecture Map

Paths below are relative to `meshtastenstein-core/` unless the heading says otherwise.

```
src/constants.rs                       — ALL portable numeric constants (frequencies, timings, sizes, crypto).
                                         Board GPIO pinouts live in the board crate, NOT here.
src/inter_task/channels.rs             — All Embassy Channel/Signal definitions + MeshEvent enum

src/domain/
  context.rs                           — MeshCtx (passed by &mut to all handlers) + ChannelMetrics
  device.rs                            — DeviceState (node num, names, modem_preset, region, channels, role)
  node_db.rs                           — NodeDB + NodeEntry (known peers)
  router.rs                            — MeshRouter: duplicate detection, rebroadcast decision, FilterResult, tick_retransmissions; PendingPacket + PendingRebroadcast structs
  radio_config.rs                      — Region + ModemPreset (proto re-exports); frequency_hz(), from_proto()
  channels.rs                          — ChannelConfig + ChannelRole (proto re-export) + ChannelSet;
                                         effective_psk() expands the 1-byte default PSK to DEFAULT_PSK
  crypto_psk.rs                        — AES-128-CTR packet encryption/decryption (channel PSK path)
  crypto_pkc.rs                        — X25519 ECDH + AES-256-CCM direct message encryption
  tx.rs                                — TxBuilder: unified LoRa frame encode + encrypt + assemble path
  packet.rs                            — RadioFrame, PacketHeader, HEADER_SIZE, BROADCAST_ADDR; with_rewritten_header()
  persistence.rs                       — NVS record layouts shared by every board's storage adapter:
                                         encode/decode only, no flash I/O (each board's I/O is sync or
                                         async depending on its flash driver, so that part can't be shared)

  handlers/
    mod.rs                             — Top-level MeshEvent dispatcher → from_radio / from_app / periodic
    from_radio/mod.rs                  — LoRa RX: try_decrypt_and_decode() + 3-layer pipeline; InboundPacket<'a> struct threaded to all portnum handlers
    from_radio/{portnum}.rs            — Per-portnum handlers: node_info, position, routing, traceroute, …
    from_app/mod.rs                    — BLE RX: decode ToRadio, config exchange, transmit_from_ble_packet
    from_app/position.rs               — BLE position save (M6)
    admin/mod.rs                       — AdminMessage dispatch from LoRa (portnum 67) addressed to us
    admin/{action}.rs                  — get_config, set_config, get_owner, set_owner, set_channel, misc
    periodic.rs                        — Tick handler: NodeInfo, NeighborInfo, position, telemetry broadcasts
    util.rs                            — Shared helpers: forward_to_ble, lora_send, send_routing_ack, push_from_radio, decode_psk_frame
    outgoing/                          — Payload builders: node_info::build_payload, telemetry::build_payload

src/tasks/
  mesh_task.rs                         — MeshState<S> (all owned fields) + MeshOrchestrator<S, R, E>
                                         (thin event pump); make_ctx() projects refs into MeshCtx
  led_task.rs                          — LED blink pattern executor. Generic over embedded-hal's
                                         OutputPin; each board wraps it in its own
                                         #[embassy_executor::task] fn (task fns can't be generic).

src/ports/                             — Trait definitions. `MeshStorage: ConfigStorage + Storage`
                                         is a marker supertrait (ports/mod.rs); the methods live on
                                         ConfigStorage (config/bond/nodedb/keypair persistence) and
                                         Storage (message ring). Plus Identity, Sleep, Reboot,
                                         EntropySource.
src/drivers/sx1262_direct.rs           — Direct SX1262 register access (sync word write).
                                         Generic over SPI/CS/BUSY embedded-hal traits, so it's
                                         shared by every board using an SX1262.
src/drivers/lora_task_body.rs          — Board-agnostic LoRa radio logic: modem-config mapping
                                         with LongFast fallback, and the whole TX/RX/CAD/
                                         channel-utilization event loop. Generic over lora-phy's
                                         `LoRa<RK, DLY>` trait bounds. Each board's own
                                         `lora_task.rs` does only SPI/GPIO setup, `LoRa::new()`,
                                         and the sync-word write, then calls `run()` here — do
                                         not duplicate the event loop into a new board's task file.
```

### NVS record layouts: `domain/persistence.rs`

All the byte-level record formats (device config, BLE bond header, PKC
keypair, message-ring header/slot framing — magic numbers, versions, field
offsets) live in `meshtastenstein-core/src/domain/persistence.rs` as plain
`encode_*`/`decode_*` functions: no flash calls, just `&[u8]` in/out. Every
board's storage adapter is a thin wrapper — its own flash-offset constants
plus I/O calls into these functions.

**This module cannot perform I/O itself**, unlike `drivers/lora_task_body.rs`.
The ESP32 adapter is built on the synchronous `embedded_storage` traits;
`mpsl::Flash` (what the nRF52 board must use, since MPSL arbitrates flash
access against radio activity) only implements the *async*
`embedded_storage_async::nor_flash::NorFlash` for writes. A board's adapter
therefore stays sync or async on its own; only the encode/decode logic is
shared. When adding a new record field, edit `persistence.rs` once — every
board picks it up automatically through its existing wrapper calls.

The BLE bond blob's magic/version header is also shared here
(`init_bond_header`/`bond_magic_valid`), even though the rest of the bond
blob depends on which `trouble-host` major a board is pinned to — see the
BLE section below.

**`ConfigStorage` and `Storage` (`ports/`) are `async fn` traits**, not
because the ESP32 needs it, but because the nRF52's `mpsl::Flash` only
implements the *async* `embedded_storage_async::nor_flash::NorFlash` for
writes/erases — MPSL arbitrates flash access against radio timeslots and
can't block. Every call site in core is already inside an `async fn`
(handler dispatch), so this costs nothing there. A synchronous adapter (the
ESP32's `NvsStorageAdapter`) is still a valid implementation — its method
bodies just never yield. `Storage`'s `is_empty`/`is_full`/`count` stay plain
sync `fn`s since they only ever read in-RAM state.

### Board crate (`boards/nrf52/`) — bring-up in progress

Done: pinout, `memory.x`, heap, port adapters (identity/entropy/reboot), MPSL +
SoftDevice Controller init, `lora_task`, `ble_task`. Not yet: NVS, battery,
watchdog, mesh orchestrator. Builds and links (213 KB flash, ~87 KB RAM with
LoRa + BLE); **never run on hardware**.

Things that cost real time to work out — don't rediscover them:

- **Pinout has three variants in the wild.** Ours is the "Wio-SX1262 for XIAO
  V1.0" / SKU 102010710 layout: CS=P0.04, DIO1=P0.03, BUSY=P0.29, RESET=P0.28,
  RXEN=P0.05, SCK=P1.13, MISO=P1.14, MOSI=P1.15. Cross-checked against upstream
  Meshtastic's `seeed_xiao_nrf52840_kit` variant and Seeed's header diagram.
  The Arduino `Dxx` numbers in both sources are logical indices, not GPIOs.
- **RF switch.** Unlike Heltec, this module needs DIO2 configured as the TX RF
  switch and RXEN asserted to receive. TCXO is 1.8 V on DIO3, same as Heltec.
  DIO2-as-switch is lora-phy's `Sx1262` default (`use_dio2_as_rfswitch()`),
  applied automatically — no extra config needed there. RXEN is a separate
  concern: despite the name it gates switch power for *both* TX and RX, not
  just RX, so `lora_task` just drives it high once at startup and leaves it.
- **The sync-word write needs a second CS/BUSY handle, same as ESP32's
  `AnyPin::steal()`.** `LoRa::new()` resets the chip internally, wiping any
  pre-init sync-word write, so it must happen *after* init — by which point
  lora-phy already owns CS/BUSY for good. embassy-nrf's `Peri` has no safe
  reborrow that survives `lora`'s lifetime (its `reborrow()` ties the borrow
  to the whole `LoRa` value, not a short window); `Peri::clone_unchecked` is
  the actual equivalent of `steal()` here — same safety argument (sequential,
  non-overlapping use), different unsafe escape hatch. Don't try to avoid the
  `unsafe` here; it was already attempted and doesn't work with this crate's
  ownership model.
- **Flash is bounded at both ends by the UF2 bootloader.** App starts at
  0x27000; the bootloader owns 0xF4000 and up. Overwriting either costs
  drag-and-drop flashing and needs an SWD probe to recover. NVS sits at
  0xEF000–0xF4000, just below the bootloader.
- **MPSL owns RADIO, TIMER0, RTC0, EGU0_SWI0, CLOCK_POWER** — hence
  `time-driver-rtc1` for Embassy, not RTC0.
- **MPSL provides `critical-section`**, via its `critical-section-impl` feature.
  Do *not* also enable `cortex-m/critical-section-single-core`: they select
  conflicting `restore-state-*` widths and the build fails. MPSL must therefore
  be initialized early, before anything takes a critical section.
- **nrf-sdc borrows the RNG peripheral** (`&'d mut`) for the controller's whole
  lifetime, so nothing else may touch it. The `EntropySource` port is a
  ChaCha20 CSPRNG seeded from the hardware TRNG *before* the controller is
  built. Do not "simplify" this into direct RNG register reads — that races
  with the controller. A fixed seed would be worse still: repeating a PKC nonce
  across reboots breaks direct-message encryption.
- **No 32 kHz crystal on this board**, so LFCLK runs from the internal RC
  oscillator (`MPSL_CLOCK_LF_SRC_RC`).
- **Flash writes go through `mpsl::Flash`**, not raw NVMC — MPSL arbitrates
  against radio activity.
- **`nrf-sdc` needs the `central` Cargo feature even for a peripheral-only
  device.** Without it, release builds fail to link with undefined symbols
  like `sdc_hci_cmd_le_create_conn_cancel` and `sdc_hci_cmd_le_enable_encryption`
  — central-role HCI commands that never actually run here, but that
  trouble-host's `Controller` trait impl for `SoftdeviceController` still
  requires the vendored `.a` to provide. Dev builds don't catch this (LTO/
  codegen-units=1 in release is what surfaces it); always verify a real
  `--release` link when touching BLE, not just `cargo check`.
- **BLE cannot share a `meshtastenstein-core` module the way `lora_task_body`
  does.** `nrf-sdc` needs `bt-hci 0.10`; `esp-radio` (checked directly, up to
  and including its 1.0 beta) is hard-pinned to `bt-hci ^0.8.0` and has no
  path off it today. `bt-hci` defines the `Controller` trait trouble-host is
  built on, so the boards are stuck on incompatible trouble-host majors whose
  API genuinely differs (`HostResources`'s generic signature changed shape
  between them). Don't re-attempt this extraction without first checking
  whether `esp-radio` has moved to a newer `bt-hci`.
- **`SoftdeviceController` skips `ExternalController` entirely** — unlike the
  ESP32's `BleConnector` (a byte-stream HCI transport), it implements
  `bt_hci::controller::Controller` directly and is passed straight to
  `trouble_host::new()`.
- **nrf-sdc licensing**: the Rust wrapper is MIT/Apache-2.0, but it links
  Nordic's precompiled SoftDevice Controller under `LicenseRef-Nordic-5-Clause`
  (Nordic silicon only, no reverse engineering).

### Board crate (`boards/esp32/`)

```
src/main.rs                            — peripheral init, NVS init (MUST be before LoRa spawn), task spawning
src/constants.rs                       — heltec_wifi_lora_v3 GPIO pinout. Mostly documentation: main.rs
                                         wires GPIOs through esp-hal's typed peripheral singletons
                                         (peripherals.GPIO8), which can't be built from a u8. Only
                                         LORA_SS/LORA_BUSY are read, by the AnyPin::steal() calls.

src/tasks/
  lora_task.rs                         — SX1262 init, TX queue, continuous RX, CAD jitter
  ble_task.rs                          — GATT server, pairing, from_radio_buf delivery, bond
  battery_task.rs                      — ADC battery level + voltage sensing
  watchdog_task.rs                     — Embassy watchdog feed
  led_task.rs                          — #[task] wrapper around core's generic led_task body

src/adapters/                          — Implementations of core's port traits:
  nvs_storage_adapter.rs               — Flash layout, SavedConfig, Bond, message ring buffer
  esp_identity_adapter.rs              — MAC-based node ID derivation
  deep_sleep_adapter.rs                — Deep sleep support
  esp_reboot_adapter.rs                — software_reset()
  esp_entropy_adapter.rs               — hardware TRNG
```

### Task spawning order (main.rs)

1. NVS init (`NvsStorageAdapter::new`) — MUST be first; loads preset/region for LoRa task.
   `DeepSleepAdapter::new` is initialized alongside it (handed to `watchdog_task` later)
2. BLE bond load (`storage.load_bond()`)
3. Device state: `DeviceState::new(&mac)` then `storage.load_state(&mut device)`
4. PKC keypair: `storage.load_pkc_keypair()`, or generate from the hardware TRNG and persist
   on first boot / after factory reset
5. LoRa params: `device.lora_params()` → `(ModemConfig, frequency_hz)`. It calls
   `Region::from_proto` internally; `ModemPreset::from_proto` is **not** on this path
6. Spawn: `lora_task` (params struct `LoraParams { is_wakeup, node_num, modem_cfg, frequency_hz }`)
7. Spawn: `esp_led_task` (board wrapper; takes an already-constructed `Output` pin, not a raw `AnyPin`)
8. Spawn: `battery_task`
9. Spawn: `ble_task` (needs `initial_bond`)
10. Spawn: `watchdog_task` (needs the `sleep` adapter from step 1)
11. `MeshOrchestrator::new(ch, &mac, storage, pkc_keypair, EspRebootAdapter, EspEntropyAdapter)`
    then `.run().await` — runs on main task (never returns)

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

**Adding a new LoRa portnum handler** (all in `meshtastenstein-core/`):
1. Create `src/domain/handlers/from_radio/my_portnum.rs` with
   `pub async fn handle<S: MeshStorage>(ctx: &mut MeshCtx<'_, S>, pkt: &super::InboundPacket<'_>)`
2. Add `pub mod my_portnum;` in `from_radio/mod.rs`
3. Add a match arm: `Some(PortNum::MyPortnum) => my_portnum::handle(ctx, &inbound).await`
4. Handler may call: `forward_to_ble`, `send_routing_ack`, update `ctx` state

All portnum handlers take `&InboundPacket<'_>` — sender, payload, packet id, channel and
SNR are fields on it, not separate parameters.

**Adding a new BLE → LoRa feature:**
- Add a portnum arm in `from_app::transmit_from_ble_packet` (or handle locally and `return` early)

---

## Key Invariants

### MeshCtx — the context struct
`MeshCtx<'_, S>` is created fresh each event loop iteration via `make_ctx()` and passed by `&mut`
to all handlers. It is a projection of `MeshState<S>` (private inner struct inside `MeshOrchestrator<S, R, E>`)
— all fields are refs/senders, no owned data. Adding a new field: edit `MeshState` + `MeshState::new()` +
`MeshOrchestrator::make_ctx()` and the `MeshCtx` struct in `domain/context.rs`.

Key fields:
- `device: &mut DeviceState` — node config, channels, role, modem_preset
- `node_db: &mut NodeDB` — known peers
- `router: &mut MeshRouter` — duplicate detection + rebroadcast state
- `pending_packets: &mut Vec<PendingPacket, 8>` — want_ack retransmit queue
- `pending_rebroadcast: &mut Option<PendingRebroadcast>` — next scheduled flood relay
- `session_passkey: &mut Option<[u8; 16]>` — `None` until first admin message (lazy init)
- `channel_metrics: &mut ChannelMetrics` — `{ channel_util: f32, air_util_tx: f32 }`
- `reboot_after_secs: &mut Option<u32>` — set by `RebootSeconds` admin; orchestrator calls `self.reboot.reboot()` (the `Reboot` port) after dispatch
- `shutdown_after_secs: &mut Option<u32>` — set by `ShutdownSeconds`; deep-sleep power-off, **not** a reboot
- `storage: &mut S` — the `MeshStorage` impl (config/bond/NodeDB/keypair + message ring)
- `entropy: &dyn EntropySource` — hardware TRNG port, used for rebroadcast jitter and PKC nonces
- `pkc_pub_bytes` / `pkc_priv_bytes: &[u8; 32]` — X25519 keypair for PKC DMs
- `my_position_bytes: &mut heapless::Vec<u8, 64>` — last position from phone or `SetFixedPosition` (RAM only)
- `tx_to_ble`, `tx_to_lora`, `led_commands` — Embassy `Sender` handles (Copy)

This list is deliberately partial — `domain/context.rs` has ~23 fields, including the
`last_*_tx: Option<Instant>` broadcast timers, `ble_connected`, `from_radio_id`,
`node_id_str` and `boot_time`. Read `domain/context.rs` before assuming a field is absent.

### LoRa RX pipeline — 3 layers in `from_radio::dispatch`
1. **Layer 0: Own-packet check** — if `header.sender == our node_num`, cancel pending ACK (implicit ACK) and drop
2. **Layer 1: FloodingRouter filter** — `router.should_filter_received()` returns `FilterResult`:
   - `New` → process normally
   - `DuplicateUpgrade(new_hop)` → upgrade pending rebroadcast, return
   - `DuplicateCancelRelay` → cancel our pending rebroadcast (another node relayed already), return
   - `DuplicateDrop` → drop, return
3. **Layer 2: Portnum dispatch** — per-portnum handler + default BLE forward + routing ACK
4. **Layer 3: Rebroadcast decision** — schedule `PendingRebroadcast` with jittered delay

Duplicate detection sizing (all in `constants.rs`, chosen to match upstream `PacketHistory`):
`DUPLICATE_RING_SIZE = 200` and **no TTL** — a match is a match regardless of age; entries
are forgotten only by oldest-first eviction when the ring fills. Do not re-add an expiry
check. `MAX_RELAYERS_TRACKED = 6` (upstream `NUM_RELAYERS`). In-RAM NodeDB is
`MAX_NODES = 96`; the NVS snapshot persists `MAX_PERSISTED_NODES = 42` (hard single-sector limit).
`rebroadcast_delay_ms()` is a free function taking a caller-supplied `raw_random: u32` —
the caller (`from_radio::dispatch`) gets it from `ctx.entropy.random_u32()` (the
`EntropySource` port), so `domain/router.rs` itself has no hardware dependency at all.

### BLE packet delivery (`boards/esp32/src/tasks/ble_task.rs`)
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
6. 14× `ModuleConfig` types: Mqtt, Serial, ExternalNotification, StoreForward, RangeTest, Telemetry, CannedMessage, Audio, RemoteHardware, NeighborInfo, AmbientLighting, DetectionSensor, Paxcounter, StatusMessage
7. NodeDB entries (one `FromRadio { node_info }` per stored node)
8. `ConfigCompleteId` (echoes the `want_config_id` from the phone's ToRadio)

### Admin message handling (`handlers/admin/`)
- All admin messages arrive as `ADMIN_APP` (portnum 67) addressed to our node num
- `admin::dispatch(ctx, sender, packet_id, payload)` decodes and routes to sub-handlers
- `SetConfig(LoRa)` → saves `region` + `modem_preset` to device state + NVS
- `SetConfig(Device)` → saves `role` to device state + NVS
- `RebootSeconds(n)` → sets `ctx.reboot_after_secs = Some(n)`; orchestrator calls the `Reboot` port after dispatch completes (`EspRebootAdapter::reboot()` wraps `esp_hal::system::software_reset()` on this board)
- Session passkey: `ctx.session_passkey` is `None` on first boot; admin handlers lazy-init via `ensure_session_passkey(ctx)`; must be echoed in all admin responses; non-empty incoming passkeys are validated against the stored key — mismatches are dropped
- `persist_config()` serializes `DeviceState` → `SavedConfig` → NVS flash

### LoRa radio parameters
- Sync word 0x2B MUST be written to SX1262 registers 0x0740/0x0741 (values 0x24/0xB4) after lora-phy init via `sx1262_direct::write_sync_word()`
- GPIO pins are `AnyPin::steal()`-ed for the direct register write; this is safe because it happens before the SPI bus is handed to lora-phy — see SAFETY comments in `boards/esp32/src/tasks/lora_task.rs`. A future board should instead hold CS/BUSY locally and do the register write before handing them over, avoiding the `unsafe` entirely.
- Frequency is computed at boot by `DeviceState::lora_params()` (`domain/device.rs`), which calls `region.frequency_hz(modem_cfg.bandwidth_hz, channel_idx)`. Note `frequency_hz` is a method on `Region` and takes a **bandwidth**, not a preset; the channel index comes from `region.default_channel_index(preset)` when `channel_num == 0`. Changing region/preset requires `RebootSeconds` + reboot because lora-phy doesn't support runtime reconfiguration

### NVS flash layout (within NVS partition)
```
0x0000–0x01FF  SavedConfig    (512 bytes, magic=0x4D434647 "MCFG", version=2)
0x1000–0x102F  BLE Bond       (48 bytes,  magic=0x424F4E44 "BOND", version=2)
0x2000–0x2A67  Message ring buffer (64-byte header + 10×260-byte slots = 2664 bytes;
                                    magic=0x4D455348 "MESH"; header: head+tail+count;
                                    each slot: 1 (valid) + 1 (len) + 255 (data) + 3 (pad))
0x3000–0x3FCF  NodeDB snapshot (4048 bytes used of the 4096-byte sector;
                                magic=0x4E444232 "NDB2", version=2;
                                16-byte header + 42×96-byte records)
0x4000–0x4047  X25519 PKC keypair (72 bytes, magic=0x504B4331 "PKC1", version=1;
                                   4-byte magic + 1-byte version + 3-byte reserved + 32-byte priv + 32-byte pub)
```

---

## Common Pitfalls

1. **`SetConfig` must save BOTH `region` AND `modem_preset`** — the app sends the full LoRa config struct even when only changing the preset. If you only save one field, the other gets corrupted on next config exchange.

2. **`RebootSeconds` must actually reboot** — the Meshtastic app always sends `RebootSeconds(N)` after any config change and waits for a reconnect. If the device doesn't reboot, Android sees a GATT_CONN_TIMEOUT (status=8) after ~30 s.

3. **MTU gotcha** — Android negotiates MTU=508 but we declare 512-byte GATT attributes. Replying with `accept()` returns all 512 bytes → phone receives 507 bytes (MTU-1) with trailing zeros → protobuf parse failure on every packet. Always use `AttRsp::Read { data: &buf[..len] }` for exact-length replies.

4. **BLE select() race** — in `ble_task.rs`, the `select` loop must gate `tx_fut` on `!from_radio_has_data`. Without this, the next packet can be pulled from the channel and overwrite `from_radio_buf` before the phone has read the current packet — silently dropping config exchange packets (app shows "no device selected").

5. **NVS init before LoRa spawn** — `main.rs` must initialize `NvsStorageAdapter` before spawning `lora_task` so the saved preset and region can be passed as parameters. The LoRa task can't be reconfigured at runtime.

6. **Default region** — the default is EU_433 (code 2), but *not* via `Region::default()`: `Region` is the prost-generated `RegionCode`, whose zero variant is `Unset`, and there is no `impl Default` for it. EU_433 comes from `DeviceState::new` hardcoding `region: 2` (a plain `u8` field) and from `Region::from_proto`'s `.unwrap_or(Self::Eu433)` fallback. `ModemPreset::default()` **is** real and is `LongFast` (code 0). The default frequency for EU_433 / LongFast is 433.875 MHz (slot 3).

7. **`esp_hal::system::software_reset()`** — NOT `esp_hal::reset::software_reset()`. The module is `system`, not `reset`. Board crate only; core reaches this through the `Reboot` port.

8. **Stack size** — `#![deny(clippy::large_stack_frames)]` is enforced on the board crate. Large stack-allocated buffers inside async functions bloat the task state machine. Use `heapless::Vec` or heap allocation instead of large arrays inside async fns. `Box<RadioFrame>` in `MeshEvent::LoraRx` and `Box<heapless::Vec<u8, 512>>` in `MeshEvent::BleRx` are intentional for this reason.

9. **`want_ack` flow** — if a packet addressed to us has `want_ack` set, we must send a routing ACK (`send_routing_ack`). If we send a packet with `want_ack`, track it in `pending_packets` for retransmission. `PendingPacket` tracks `is_our_packet` and on last retry clears `next_hop` to fall back to flooding.

10. **Never add a chip dependency to `meshtastenstein-core`.** If domain or handler code needs something from the hardware — a reset, randomness, a clock, a pin — add a port trait in `ports/` and implement it per board. Three such couplings (`esp_hal` reset and two TRNG calls) had already leaked into "portable" code before the split and had to be pulled back out.

---

## Debugging Tips

- **Serial log level**: set `RUST_LOG=debug` env var before build; `esp_println` reads it at boot
- **Android adb logcat**: `adb logcat -s BluetoothGatt geeksville.mesh` — shows MTU negotiation, connection state, GATT reads/writes
- **Status codes**: Android `onClientConnectionState` status=8 = GATT_CONN_TIMEOUT (device vanished), status=22 = peer terminated, status=0 = success
- **Protobuf decode failures**: if BLE FromRadio payloads are malformed on the phone, check `from_radio_len` — should never be 0 or exceed actual encoded length
- **Frequency verify**: log line `[LoRa] Entering continuous RX mode at X Hz` — cross-check with the expected formula: `region.freq_start_hz() + bw/2 + ch * bw`, where `ch = djb2(preset.display_name()) % (region.band_hz() / bw)`. For EU_433 + LongFast this gives 433 000 000 + 125 000 + 3 × 250 000 = **433.875 MHz**

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

