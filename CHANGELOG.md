# Changelog

## 2026-08-25

### Changed
- **The firmware is now split into a hardware-agnostic core and a per-board binary**, so a second board can be added without touching the protocol. Everything that isn't chip-specific — the mesh protocol, routing, crypto and persistence — is shared; each board supplies its own radio, BLE, flash, battery and watchdog support. Boards are built and released independently, since they need different compilers. Behaviour on the Heltec ESP32-S3 is unchanged.
- **Reboots and random-number generation are now board-supplied** rather than assuming an ESP32. This covers the reboot an admin message triggers, the jitter before relaying a packet, and the nonce protecting encrypted direct messages.

---

## 2026-08-13

### Fixed
- **PKC direct messages were undecryptable by real Meshtastic nodes** — we used the raw X25519 ECDH output as the AES-256-CCM key, where upstream SHA-256-hashes it first, so the two sides derived different keys. Both the encrypt and decrypt paths now share one key-derivation function.

### Added
- **Node-list admin actions** — favorite, ignore and mute a node, plus adding a contact from a shared contact record. Ignoring also scrubs the cached position and public key, and new contacts are auto-favorited so they survive eviction, both matching upstream. The three flags are mirrored to the phone and persisted in the NodeDB snapshot without a schema bump.
- **Module config get/set over admin** — `Get` returns the same defaults sent during config exchange for every module type the firmware exposes; previously it fell through as unhandled and the phone's module-config screens appeared to do nothing. `Set` is acknowledged but not persisted, since nothing reads the values back yet.
- **Fixed position set/remove over admin** — reuses the existing position broadcast mechanism, so a fixed position immediately becomes the position broadcast on the mesh. Not flash-persisted, same as phone-pushed positions.
- **Status message module config added to config exchange** — upstream sends 14 module config types; we sent 13.
- **BLE fast-connection-interval request on connect** — requests a short interval right after a phone connects, matching upstream, which speeds up the initial config-exchange burst. Best-effort: some phones ignore peripheral-initiated updates.

### Changed
- **Removed two dead constants holding a stale default channel index and frequency** — both described a hash algorithm the code had already replaced, and neither was referenced anywhere. The default channel and frequency are computed from the region and preset at boot, so the hardcoded copies were both wrong and unused.
- **Rebroadcast jitter no longer reaches for the hardware RNG inside the router** — the caller now supplies the random value, so the routing and dedup logic is free of any hardware dependency. Same random source and distribution; a prerequisite for host-testing that logic later.
- **Traceroute replies now go through the shared transmit builder** — the handler had hand-rolled its own encode, encrypt and frame-assembly sequence duplicating what the builder already does. Identical wire behaviour.
- **In-RAM NodeDB capacity raised 64 → 96** — upstream's ESP32-S3 default is 100; a conservative value was chosen because node entries draw on a fixed heap shared with BLE buffers and the transmit queue, and this dev environment can't cross-compile to verify actual headroom. The persisted count stays at 42, already the maximum that fits one flash sector.
- **Duplicate-detection cache enlarged to 200 entries, TTL removed, relay cancellation role-gated** — upstream has no expiry at all (a match is a match regardless of age; entries are forgotten only by oldest-first eviction) and sizes the cache to 200 on comparable boards. Router-class roles are now exempt from cancelling a pending rebroadcast, so they always relay.
- **Rebroadcast delay is now channel-utilization and SNR weighted**, replacing a flat jitter with upstream's contention-window formula, including its role gate that lets router traffic relay first. On a busy mesh this should reduce collisions with stock nodes.
- **Preamble extended 16 → 64 symbols** on both TX and RX. Stock 16-symbol receivers still detect it, so interop is preserved; the wider RX window increases detection margin during the deep-sleep wake transition, at the cost of extra airtime.
- **Verbose IRQ logging added to the deep-sleep wake-packet read** — diagnostic only, so a debug serial capture during a hardware wake test shows the full IRQ timeline rather than just the outcome.

---

## 2026-05-22

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

## 2026-04-30

### Changed
- **Dependency updates** — `embassy-sync` 0.7.2 → 0.8.0, `embassy-embedded-hal` 0.5.0 → 0.6.0, `trouble-host` crates.io → git main (embassy-sync 0.8 support), esp-hal 1.1.0-rc.0 → 1.1.0.
- **BLE bond blob version 2** — `BOND_VERSION` bumped 1 → 2; `Identity.addr` is now `Address { kind, addr }`; old blobs discarded (one-time re-pairing required).

---

## 2026-04-17

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
