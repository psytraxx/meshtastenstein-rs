# Changelog

## [Unreleased] — 2026-05-22

### Added
- **PKI routing errors for failed PKC DMs** — when a PKC direct message fails decryption (stale sender key after keypair regeneration) or the sender's public key is absent from NodeDB, we now send `Routing.PKI_FAILED` / `Routing.PKI_UNKNOWN_PUBKEY` back to the sender so the remote app shows an error instead of silently timing out; added `send_routing_error()` helper and `DecryptOutcome` enum to distinguish failure modes.
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
