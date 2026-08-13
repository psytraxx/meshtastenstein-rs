# Changelog

## [Unreleased] — 2026-08-13 (architecture review follow-up)

### Changed
- **`MeshRouter`'s rebroadcast-jitter RNG call moved out of `router.rs`** — `random_below()` previously called `esp_hal::rng::Rng::new().random()` directly inside the router module, meaning routing/dedup logic (otherwise pure, hardware-independent state-machine code) had a hidden ESP32 HAL dependency. `rebroadcast_delay_ms()` now takes a caller-supplied `raw_random: u32`; the one call site (`from_radio/mod.rs`) draws it from the HAL and passes it in. No behavior change — same random source, same distribution — but `router.rs` no longer imports `esp_hal` at all, which is a prerequisite for ever host-testing the routing logic (not done in this pass; deferred pending a decision on a `std`-feature vs. workspace-split test harness).
- **`traceroute.rs`'s reply path rewritten to use `TxBuilder`** — the handler previously hand-rolled its own encode → channel/PSK lookup → encrypt → header-build → frame-assembly sequence (~65 lines), duplicating what `TxBuilder::build()` already does and already uses identically for `send_routing_ack`'s "reply on the same channel the request arrived on" case. Cut to ~40 lines with identical wire behavior (same hop_limit/hop_start, same channel selection, same PSK-vs-plaintext gating) — verified by comparing `TxBuilder`'s defaults and internal `make_flags`/channel-lookup logic field-by-field against the code it replaced.

### Diagnostics
- **Added per-iteration IRQ status logging to the deep-sleep wake-packet read** (`sx1262_direct.rs`'s `read_wake_packet` poll loop) — previously only the terminating condition (RxDone found, RX timeout, or final "timed out" warning) was logged; now every poll iteration logs the raw IRQ status bytes at `debug!` level. This doesn't change behavior — it exists so a `RUST_LOG=debug` serial capture during a hardware wake-on-LoRa test shows the full IRQ timeline across the (up to 4-second) wait, which is needed to diagnose the still-open question of whether a second incoming packet during this window can silently overwrite the SX1262's RX buffer before it's read (see README Known Limitations: "Deep-sleep wake-on-LoRa reliability").
- **Added two new hardware test checklist rows** (README, P1 — Power Management): a VEXT-vs-SX1262 power-domain multimeter check (settles whether cutting VEXT before sleep also kills the radio — currently unverified either way), and a rapid multi-packet wake test with precise pass/fail criteria (send 3 packets ~500ms apart to a sleeping node, confirm all 3 are recovered). Both were previously only described in prose in Known Limitations, not present as executable checklist items.

## [Unreleased] — 2026-08-13

### Fixed
- **PKC direct messages used the raw ECDH shared secret as the AES-256-CCM key** — upstream Meshtastic hashes the X25519 shared secret with SHA-256 (`CryptoEngine::setDHPublicKey` + `hash()`) before using it as the key; we used the raw 32-byte ECDH output directly. This made every PKI-encrypted direct message undecryptable by (and sent from) real Meshtastic nodes, since the two sides derived different keys. `derive_shared_key()` in `crypto_pkc.rs` now SHA-256-hashes the ECDH output to match; both call sites (`tx.rs` encrypt path, `from_radio/mod.rs` decrypt path) go through this one function so the fix applies uniformly. Added the `sha2` crate (`no_std`, RustCrypto).

### Added
- **Node-list admin actions**: `SetFavoriteNode`/`RemoveFavoriteNode`, `SetIgnoredNode`/`RemoveIgnoredNode` (also scrubs cached position + public key, matching upstream), `ToggleMutedNode`, `AddContact` (creates/updates a NodeDB entry from a `SharedContact`; auto-favorites new contacts so they survive eviction, or marks ignored + scrubs state when `should_ignore` is set — mirrors upstream `NodeDB::addFromContact`, minus the `CLIENT_BASE`-role special case and manual-key-verification gate, neither of which exist in this codebase). New `NodeEntry` fields `is_favorite`/`is_ignored`/`is_muted`, mirrored to the phone via `NodeInfo.is_favorite`/`is_ignored`/`is_muted`. Persisted in the NVS NodeDB snapshot using the previously-reserved byte 19 as a 3-bit flags field — no schema/version bump needed, since old records have byte 19 = 0 (all flags false), which is the correct default (`node_db.rs`).
- **`GetModuleConfigRequest` / `SetModuleConfig`** — `Get` now returns a real `GetModuleConfigResponse` for each of the 14 `ModuleConfigType`s this firmware exposes during config exchange (same default-valued structs); previously fell through to "Unhandled admin variant" and the Android app's module-config screens would appear to do nothing. `Set` is acknowledged (no longer silently dropped) but not persisted, since no per-module behavior reads these values back yet — same honesty as the existing `SetConfig` handler's catch-all branch for unsupported variants.
- **`SetFixedPosition` / `RemoveFixedPosition`** — reuses the existing `my_position_bytes` broadcast mechanism (same field the phone's own `PositionApp` push already populates), so a fixed position set via admin immediately becomes the position broadcast on the mesh. Not flash-persisted (see Known Limitations) — same as phone-pushed positions.
- **`StatusMessage` added to the config-exchange `ModuleConfig` list** — was missing; upstream sends 14 `ModuleConfig` types, we only sent 13.

### Documentation
- **README "What's Left / Known Limitations" reconciled against the 2026-08-13 audit fixes** — added rows for the `CLIENT_BASE`-role semantics and manual-public-key-verification gaps deliberately not ported during the `AddContact`/node-actions work (see above), and for the deep-sleep wake-on-LoRa reliability question (unverified on real hardware; upstream deliberately avoids this exact approach — see code comment cited in `sleep.cpp`). Downgraded the "Wake from deep sleep on LoRa RX" feature-matrix row from ✅ to ⚠️ since the mechanism is implemented but its reliability hasn't been confirmed on a physical device.

### Added
- **BLE fast-connection-interval request on connect** — `ble_task.rs` now calls `Connection::update_connection_params()` immediately after a phone connects, requesting a 7.5–15ms interval / no slave latency / 2s supervision timeout. Matches upstream's `onConnect` `updateConnParams(6, 12, 0, 200)`, which speeds up the initial config-exchange burst (many small BLE reads/writes back-to-back). Best-effort: some phones/OSes reject or ignore peripheral-initiated connection parameter updates, so a failure here is logged at debug level and otherwise ignored.

### Notes (re-assessed from the 2026-08-13 firmware-parity audit's BLE findings)
- **Outbound BLE buffering "gap" was a mischaracterization** — the audit compared our single-slot `from_radio_buf` (inside `gatt_events_loop`) against upstream's queue and called it a 3-deep-vs-1-deep gap. In fact upstream's GATT-facing slot (`PhoneAPI::packetForPhone`) is *also* single-slot, for the same reason ours is: a GATT characteristic can only hold one "current value" until the phone reads it. The real queue depth to compare is upstream's `MeshService::toPhoneQueue` (`MAX_RX_TOPHONE` = 32 on ESP32-S3) against our `ble_tx` Embassy channel (`inter_task/channels.rs`, depth 48) — ours is already deeper, not shallower.
- **App-level bond persistence "gap" was also a mischaracterization** — `trouble-host`'s built-in bond store (`HostResources::bond_storage`) is in-RAM only; the crate has no flash/NVS persistence of its own to delegate to. Serializing `BondInformation` to NVS ourselves (`serialize_bond`/`deserialize_bond` in `ble_task.rs`, restored via `add_bond_information()` on boot) is the only way to get bond persistence across reboots with this BLE stack — not a shortfall relative to some stack-native alternative that doesn't exist.
- **`LogRadio` characteristic remains unimplemented** — genuinely absent, but it's a diagnostic firmware-log-to-phone feature with no mesh-protocol data flowing through it (see Known Limitations in README). Left out of scope given low value relative to the work (new characteristic + wiring `log` crate output through a new channel) and that serial logging already covers debugging needs per this project's conventions.

### Changed
- **`MAX_NODES` (in-RAM NodeDB capacity) raised 64 → 96** — upstream's ESP32-S3 default is 100. Chose a conservative value rather than matching upstream exactly: `NodeEntry` holds heap-backed `Option<User>`/`Option<Position>` fields drawn from a fixed 72 KB heap shared with BLE buffers, the LoRa TX queue, and crypto scratch space, and this dev environment can't cross-compile for the Xtensa target to verify actual heap headroom. `MAX_PERSISTED_NODES` (NVS-persisted, currently 42) was left unchanged — it's already the maximum that fits in one 4 KB flash sector at 96 bytes/record (16-byte header + 42×96 = 4048 of 4096 bytes); raising it needs either a smaller record format (version bump) or spanning a second sector, both judged not worth the complexity for this low-severity gap.
- **Duplicate-detection cache enlarged to 200 entries, TTL removed, relay cancellation now role-gated** — `DUPLICATE_RING_SIZE` was a fixed 64 with a 1-hour expiry; upstream's `PacketHistory` has no TTL at all (a match is a match regardless of age; entries are only forgotten via oldest-first eviction when the ring fills) and sizes itself to `max(MAX_NUM_NODES*2, 100) = 200` on ESP32-S3-class boards. Removed the age check in `find_record()` (and the now-unused `seen_at_ms` field) and bumped `DUPLICATE_RING_SIZE` to 200 (`constants.rs`; ~4.8 KB additional static RAM, trivial on this target). Also bumped `MAX_RELAYERS_TRACKED` from 4 to 6 to match upstream's `NUM_RELAYERS`. Separately, `should_filter_received()` previously canceled a pending rebroadcast unconditionally whenever another node was heard relaying first; upstream's `roleAllowsCancelingDupe()` exempts ROUTER and ROUTER_LATE roles from ever canceling, so they always rebroadcast. Ported that gate (`DeviceRole::Router | DeviceRole::RouterLate` now bypass `DuplicateCancelRelay`); the `CLIENT_BASE`+favorited-node exemption from upstream was not ported since favorite-node tracking isn't implemented in this firmware yet.
- **Rebroadcast delay now channel-utilization/SNR-weighted, matching upstream `getTxDelayMsecWeighted`** — replaced the flat `100ms + snr*10ms` jitter with upstream's full contention-window formula: `slot_time` derived from spreading factor + bandwidth (`RadioInterface::computeSlotTimeMsec`), an SNR-mapped contention-window exponent in `[CW_MIN=3, CW_MAX=8]` (`getCWsize`), and a role-gated delay — ROUTER nodes get a short `random(0, 2*CWsize) * slot_time` window, all other roles wait a fixed `2*CW_MAX*slot_time` offset plus their own random window, so ROUTER traffic relays first. Implemented as a free function `rebroadcast_delay_ms()` in `router.rs` (replaces the old `MeshRouter::rebroadcast_delay_ms` method); new constants `CW_MIN`, `CW_MAX`, `SNR_MIN_DBM`, `SNR_MAX_DBM`, `NUM_SYM_CAD`, `SLOT_TIME_FIXED_MS` in `constants.rs`. On a busy real mesh this should reduce rebroadcast collisions with stock Meshtastic nodes relative to the old fixed delay.
- **Extended preamble length (16 → 64 symbols)** — `MESHTASTIC_PREAMBLE_LENGTH` now feeds a 64-symbol preamble on both TX and RX (`constants.rs`), up from the Meshtastic-standard 16. A longer TX preamble is still detected by stock 16-symbol receivers (preamble detection locks on once enough symbols accumulate; no exact length match is required), so interop with stock Meshtastic nodes is preserved. The wider RX window increases the detection margin during the deep-sleep wake-on-LoRa transition, at the cost of extra per-packet airtime.

## [Unreleased] — 2026-05-22

### Added
- **PKI routing error for unknown sender** — when a PKC direct message arrives but the sender's public key is absent from NodeDB, we send `Routing.PKI_UNKNOWN_PUBKEY` back (matches official firmware); added `send_routing_error()` helper and `DecryptOutcome` enum to distinguish failure modes.
- **Proactive NodeInfo on PKC decrypt failure** — when a PKC DM fails CCM tag verification (sender has stale public key for us), we silently drop the packet (aligning with official firmware) and immediately unicast our current `NodeInfo` to the sender so they can refresh our public key and retry; diagnostic log now shows the first 4 bytes of our public key vs. the sender's cached copy of it.
- **`MeshEvent::BondClear`** — `ble_task` sends this on `PairingFailed`; handler erases the NVS bond so the next boot pairs fresh.

### Fixed
- **Bond-clear doesn't recover** — `PairingFailed` cleared the NVS bond but the in-memory BLE stack kept the old bond, causing every subsequent connect to fail again. Now `ble_task` calls `software_reset()` after the disconnect so the stack reloads bond-free and the phone can pair fresh.
- **`software_reset()` loses race against watchdog sleep** — runner error handler used `Timer::after(200ms)` before resetting; the watchdog's 500ms grace period expired first when phone reconnected ~300ms into the window, so deep sleep won. Reduced both delays to 50ms so reset fires well before the grace period ends.
- **`load_slots()` reads uninitialized flash as valid** — 0xFF valid byte (erased NOR flash default) compared `!= 0` → true, loading garbage frames. Changed to `== 1`.
- **Sleep-while-connected** — `BleConnected` didn't signal activity, so the watchdog fired deep sleep immediately after the phone reconnected post-disconnect. Fixed by calling `activity.signal()` in `next_event()` for `BleConnected`.
- **Store-and-forward never delivered after sleep** — slot data was never persisted to flash, so after a wake `peek()` always returned `Err` and `pop()` was never called, leaving `count` stuck at 1 forever. Fixed by adding per-slot flash persistence in `add()` / `pop()` and restoring all slots in `load_or_init()`.
- **Wake-packet verbose logging** — added header field dump and mesh_in queue result to trace the LoRa wake packet path end-to-end.
- **Outgoing reactions missing `emoji`/`reply_id`** — `TxBuilder` didn't have these fields so OTA reactions arrived as plain text. Fixed by adding them to `TxBuilder` and extracting them in `transmit_from_ble_packet`.
- **Incoming reactions forwarded as plain text** — `reply_id`/`emoji` were zeroed by `..Default::default()` in `make_from_radio_packet`. Fixed by threading them through `DecodedPayload` → `InboundPacket` → `PacketForwardArgs`.
- **BLE runner panic on rapid reconnect** — `runner.run().await.unwrap()` panicked with `BleHost(InvalidState)` on reconnect race. Replaced with graceful error + `software_reset()`.
- **BLE bond version mismatch** — `BOND_VERSION` was 1 in the adapter but 2 in `ble_task`, causing every stored bond to be rejected. Aligned both to 2.
- **Deep sleep inactivity timer never firing** — relayed echoes of our own broadcasts reset `last_activity`. Fixed by skipping `activity.signal()` when `sender == my_node_num`.
- **Stub node names for unknown peers** — `make_node_info_from_radio` now synthesises `User { short_name, long_name }` when `NodeEntry.user` is `None`, matching the official firmware convention.
- **Stub node BLE push flooding** — `notify_ble_node_update` now only fires when `is_new_node` is true.

### Changed
- **Protobuf submodule** — new `ModemPreset` variants (`LiteFast`, `LiteSlow`, `NarrowFast`, `NarrowSlow`) and `RegionCode` variants; added RF parameters to all exhaustive match blocks in `radio_config.rs`.

---

## [Unreleased] — 2026-04-30

### Changed
- **Dependency updates** — `embassy-sync` 0.7.2 → 0.8.0, `embassy-embedded-hal` 0.5.0 → 0.6.0, `trouble-host` crates.io → git main (embassy-sync 0.8 support), esp-hal 1.1.0-rc.0 → 1.1.0.
- **BLE bond blob version 2** — `BOND_VERSION` bumped 1 → 2; `Identity.addr` is now `Address { kind, addr }`; old blobs discarded (one-time re-pairing required).

---

## [Unreleased] — 2026-04-17

### Added
- **Admin session passkey validation** — non-empty incoming passkeys validated against stored passkey; mismatches dropped.
- **NodeDB schema v2** — record size 64 → 96 bytes; X25519 peer public key persisted at bytes 64..96; magic `NDB2`; `MAX_PERSISTED_NODES` 48 → 42.

### Changed
- **`pending.rs` folded into `router.rs`** — `PendingPacket` and `PendingRebroadcast` moved; `pending.rs` removed.
- **`InboundPacket<'a>` struct** — uniform `handle(ctx, &InboundPacket)` signature across all 9 portnum handlers.
- **Store-and-forward moved to dispatch** — TEXT_MESSAGE buffering lifted from `text_message::handle` to `from_radio::dispatch`.
- **`ToRadioMessage` wrapper removed** — `MeshEvent::BleRx` carries `Box<heapless::Vec<u8, 512>>` directly.
- **`PortNum::XxxApp.into()`** — replaced all `as i32` casts with `.into()`.
- **`hex_byte` helper deduplicated** — added to `handlers/util.rs`.
- **`learn_route` simplified** — removed `record_our_transmission`; `learn_route` unconditionally writes `next_hop` when a relay is observed.

### Removed
- `src/domain/pending.rs`, `ToRadioMessage`, `record_our_transmission`, `PacketRecord::our_hop_limit`

---

## [Phase 1+2] — 2026-04-15

### Added
- **X25519 PKC direct messages** — ECDH + AES-256-CCM; keypair from TRNG, persisted to NVS; auto-selected for unicast DMs.
- **NodeDB persistence (v1)** — top-48 nodes snapshotted to NVS; debounced 5-min flush + pre-sleep flush.
- **Deep sleep** — inactivity watchdog (5 min), low-battery auto-sleep (< 5% SoC), DIO1/button wakeup.
- **Regulatory duty-cycle TX gating** — per-region polite + hard ceilings; rolling 1-hour airtime window.
- **Congestion-scaled periodic broadcasts** — NodeInfo (3 h), Position (15 min), Telemetry (60 min), NeighborInfo (6 h).
- **Multi-channel support** — up to 8 channels, per-channel PSK, channel-aware ACK routing.
- **Store-and-forward** — TEXT_MESSAGE frames buffered in NVS ring while BLE disconnected; replayed on reconnect.
- **Traceroute, NeighborInfo, Battery telemetry, Admin commands** (ShutdownSeconds, FactoryReset, NodeDBReset, etc.)

### Changed
- `MeshOrchestrator` → `MeshState<S>` + thin event-pump wrapper; `make_ctx()` projects into `MeshCtx<'_, S>`.
- `session_passkey` → `Option<[u8; 16]>` (lazy init).
- `ChannelMetrics` sub-struct introduced.

---

## [Phase 0] — initial

### Added
- Embassy async task skeleton, SX1262 LoRa init + CAD-jittered rebroadcast, sync word 0x2B.
- Meshtastic GATT service (ToRadio / FromRadio / FromNum), MTU-correct replies, PIN pairing, bond persistence.
- Full config exchange, FloodingRouter, NextHopRouter, ReliableRouter, AES-128-CTR PSK encryption.
- NodeDB (in-memory), NVS persistence (SavedConfig + BLE bond), Admin (GetOwner/SetOwner, GetConfig/SetConfig, GetChannel/SetChannel, RebootSeconds).
