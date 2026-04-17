# Changelog

## [Unreleased] — 2026-04-17

### Added
- **Admin session passkey validation** — non-empty incoming passkeys now validated against the stored passkey; mismatches are dropped with a warning (`handlers/admin/mod.rs`)
- **NodeDB schema v2** — `SNAPSHOT_RECORD_SIZE` expanded 64 → 96 bytes; X25519 peer public key (32 bytes) persisted at bytes 64..96; all-zero = not known; magic bumped to `NDB2`, version to 2; `MAX_PERSISTED_NODES` reduced 48 → 42 to keep the snapshot within one 4 KB NVS sector (`node_db.rs`)

### Changed
- **`pending.rs` folded into `router.rs`** — `PendingPacket` and `PendingRebroadcast` now live in `src/domain/router.rs`; `src/domain/pending.rs` removed
- **`InboundPacket<'a>` struct** — introduced in `from_radio/mod.rs`; all 9 portnum handlers share a uniform `handle(ctx, &InboundPacket)` signature; removed `#[allow(clippy::too_many_arguments)]` from traceroute handler
- **Store-and-forward moved to dispatch** — TEXT_MESSAGE buffering to NVS moved from `text_message::handle` up to `from_radio::dispatch` where the `RadioFrame` is in scope; `frame` field removed from `InboundPacket`
- **`ToRadioMessage` wrapper removed** — `MeshEvent::BleRx` now carries `Box<heapless::Vec<u8, 512>>` directly; intermediary struct eliminated (`inter_task/channels.rs`, `ble_task.rs`, `from_app/mod.rs`)
- **`PortNum::XxxApp.into()` everywhere** — replaced all `PortNum::XxxApp as i32` casts with `.into()` (prost derives `From<PortNum> for i32`)
- **`hex_byte` helper deduplicated** — `pub const fn hex_byte(b: u8) -> [char; 2]` added to `handlers/util.rs`; `DeviceState::new()` and `build_node_id_string` both use it
- **Routing fix: `learn_route` simplified** — removed `record_our_transmission` (outgoing packets were never findable in the receive-ring, so the two-way check always failed); `learn_route` now unconditionally writes `NodeEntry::next_hop` when a relay is observed

### Removed
- `src/domain/pending.rs` (contents merged into `router.rs`)
- `inter_task::channels::ToRadioMessage` struct
- `MeshRouter::record_our_transmission` method
- `PacketRecord::our_hop_limit` field

---

## [Phase 1+2] — 2026-04-15

### Added
- **X25519 PKC direct messages** — Curve25519 ECDH + AES-256-CCM matching upstream `encryptCurve25519`; keypair generated from hardware TRNG on first boot, persisted to NVS sector 4; peer public keys cached from received NodeInfo broadcasts; auto-selected for unicast DMs when peer key is in NodeDB
- **NodeDB persistence (v1)** — top-48 nodes snapshotted to NVS sector 3; restored on boot; debounced 5-min flush + forced pre-sleep flush via `ShutdownSeconds` path
- **Deep sleep** — inactivity watchdog (5 min), low battery auto-sleep (< 5% SoC), DIO1/button wakeup; `ShutdownSeconds` admin command routes through watchdog task
- **Regulatory duty-cycle TX gating** — per-region polite + hard ceilings (1% EU_868, 10% EU_433, unlimited US); rolling 1-hour airtime window
- **Congestion-scaled periodic broadcasts** — NodeInfo (3 h), Position (15 min), Telemetry (60 min), NeighborInfo (6 h); intervals scale with online node count
- **Multi-channel support** — up to 8 channels (1 primary + 7 secondary), per-channel PSK encryption, channel-aware ACK routing
- **Store-and-forward** — TEXT_MESSAGE frames buffered in NVS ring while BLE disconnected; replayed after next config exchange
- **Traceroute** — appends node SNR + node_num to `RouteDiscovery`, returns response on same channel
- **NeighborInfo** — RX decoded, neighbor SNR logged, NodeDB-touched, BLE forwarded; TX every 6 h
- **Battery telemetry** — ADC sampling with OCV lookup table, broadcast as TELEMETRY_APP via LoRa and BLE GATT 0x180F
- **Admin: ShutdownSeconds, FactoryReset, NodeDBReset, RemoveNodeByNum, BeginEditSettings, CommitEditSettings**

### Changed
- `MeshOrchestrator` refactored into `MeshState<S>` (owned fields) + thin event-pump wrapper; `make_ctx()` projects refs into `MeshCtx<'_, S>`
- `session_passkey` changed from `([u8; 16], bool)` pair to `Option<[u8; 16]>` (lazy init)
- `ChannelMetrics` sub-struct introduced (`channel_util: f32`, `air_util_tx: f32`) replacing two parallel scalar fields

---

## [Phase 0] — initial

### Added
- Embassy async task skeleton: `mesh_task`, `lora_task`, `ble_task`, `battery_task`, `led_task`, `watchdog_task`
- SX1262 LoRa init + continuous RX + CAD-jittered rebroadcast; sync word 0x2B via direct register write
- Meshtastic GATT service (ToRadio / FromRadio / FromNum), MTU-correct read replies, PIN pairing, bond persistence
- Full config exchange sequence (MyNodeInfo → ConfigCompleteId)
- FloodingRouter: 64-entry duplicate ring, hop-limit upgrade, relay cancellation, role-based skip (ClientMute/ClientHidden)
- NextHopRouter: `next_hop` lookup + route learning from relay_node field
- ReliableRouter: want_ack queue, 3 retries × 5 s, fallback-to-flood, implicit ACK on own rebroadcast
- AES-128-CTR channel PSK encryption/decryption
- NodeDB: in-memory, stale eviction, hops_away tracking, NodeInfo/Position/Routing/Telemetry portnum handlers
- NVS persistence: SavedConfig (names, region, modem preset, role, channels) + BLE bond
- Admin: GetOwner/SetOwner, GetConfig/SetConfig (LoRa + Device), GetChannel/SetChannel, RebootSeconds
