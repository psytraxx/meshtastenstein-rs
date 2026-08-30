# Changelog

## 2026-08-30

### Changed
- **The protocol definitions were updated to Meshtastic 2.8.0**, and the firmware version reported to the phone app now matches. The minimum app version required to connect also rose to match upstream, so a very old Meshtastic app build may need updating before it can pair.

### Added
- **LoRa hop limit and TX enable/disable are now configurable from the app**, and take effect immediately without a reboot — previously they were accepted and acknowledged but silently discarded, so the app's own settings had no real effect.
- **Custom LoRa modem parameters (spreading factor, bandwidth, coding rate) and an explicit channel number are now stored and applied** when the app requests them instead of a preset. An individual out-of-range value falls back to its own safe default rather than the whole custom configuration being discarded, matching how the standard app's own firmware handles the same case.
- **Four new amateur-radio LoRa regions and three new modem presets from upstream are now supported**, with their correct frequencies and radio parameters.

### Fixed
- **The phone app was always told the device's role was "Client" during the initial connection handshake**, regardless of the role actually configured, and only showed the correct role after a separate explicit request. The initial handshake now reports the real role immediately.
- **Requesting the current LoRa configuration reported hardcoded placeholder values** (a fixed hop limit, "custom parameters" always reported as a preset, TX always reported as disabled) instead of what was actually configured.

## 2026-08-28

### Fixed
- **A single relay slot could silently drop a mesh packet the node should have rebroadcast.** Only one pending rebroadcast could be queued at a time; a second relayable packet arriving before the first one's jittered delay elapsed replaced it instead of queuing alongside it. On a busy mesh this meant this node quietly stopped relaying a share of traffic while otherwise appearing healthy. The relay queue now holds several pending rebroadcasts at once.
- **A broadcast message with delivery confirmation requested could make every node that heard it reply at once.** The acknowledgment check treated "addressed to us" as including broadcasts, so an incoming broadcast asking for an ACK triggered a reply from every receiver on the mesh simultaneously, rather than only from the intended unicast recipient.
- **This node broadcast its presence, position, and telemetry noticeably more often than a standard Meshtastic node on small or quiet meshes** — up to 67% more frequently — because the interval-scaling logic shortened intervals below their configured base on a small mesh instead of only ever lengthening them on a large one.
- **An admin command received over the mesh radio was carried out, but its response was always delivered to the locally connected phone instead of back to the node that sent it** — so a remote administration request (e.g. a config change or reboot) silently applied while appearing to time out for the requester. Responses now go back over whichever transport — phone or mesh radio — the request arrived on.
- **The admin session passkey was derived deterministically from the node's own number** (which is broadcast in every packet) **instead of drawn from the hardware random source**, and was also the wrong size for the standard app to recognize. It's now 8 random bytes with a 5-minute expiry, matching the standard protocol.
- **Marking a node as a favorite, ignoring it, muting it, or removing it from the node list didn't reliably survive a reboot.** These changes updated the in-memory node list but didn't always flag it for the next flash write, so they could be lost if no other change happened to trigger a save first.
- **An admin message addressed to a different node could be relayed to a locally connected phone twice.**
- **The phone app was told a message was delivered as soon as it was handed to the radio, before the mesh had actually attempted delivery.** For messages requesting delivery confirmation, the app now learns the real outcome instead: a genuine acknowledgment from the destination, or a failure notice once retries are exhausted with no reply — matching how the app expects delivery status to be reported.
- **The list of known nodes sent to a newly connected phone was capped well below the number of nodes actually tracked**, so on a busy mesh some known nodes could be silently missing from the phone's view even though the device still had them and could still route to them.

## 2026-08-26

### Changed
- **The radio now sleeps between listens instead of receiving continuously**, on both boards. This uses the SX1262's own hardware duty-cycle mode — the radio autonomously alternates a short listen window with sleep and only wakes the host on an actual incoming transmission — cutting idle radio current by roughly 84% with no change in how reliably packets are received. A background timer that previously interrupted reception every 30 seconds for routine bookkeeping has been reworked to avoid disrupting the radio's sleep cycle.
- **The two boards' flash storage adapters were consolidated into a single shared implementation.** Both boards persisted device config, BLE bonds, buffered messages, the node database, and the PKC keypair with nearly identical logic, differing only in the underlying flash driver. That logic now lives in one place shared by both boards, with each board supplying only its own flash driver and the location of its storage region.

### Fixed
- **The nRF52840 board no longer powers itself down after five minutes of mesh inactivity.** That shutdown mode has no way to wake back up on this board, so an idle-but-healthy node would silently and permanently drop off the mesh until someone physically reset it — the same trigger is harmless on the ESP32 board, which wakes back up on the next radio packet. The board still powers down on an explicit admin request or critically low battery.

### Added
- **The mesh orchestrator now runs on the nRF52840 board.** With flash storage in place, the board can generate or restore its device identity and PKC keypair, then wire LoRa and BLE into the same mesh protocol loop the ESP32 board runs — the last gap keeping this board from joining a mesh end-to-end.
- **Battery monitoring and a hardware watchdog on the nRF52840 board**, closing the last feature gap with the ESP32 board. Battery level now reads from the same VBAT sense circuitry upstream's own firmware uses for this hardware; the watchdog periodically feeds a 90-second hardware timeout and, like the ESP32 board, will disconnect BLE and power the device off on an admin-requested shutdown, low battery, or inactivity timeout — matching upstream's nRF52 behavior, which powers off rather than entering the ESP32's wake-on-LoRa deep sleep.
- **An RX-sensitivity register patch and an explicit transmit current limit are now applied to the SX1262 on both boards**, matching an undocumented Heltec/Semtech recommendation and upstream's own override of a conservative library default. Neither was set before.

### Fixed
- **The nRF52840 board's antenna switch was held in a single fixed state instead of being switched between transmit and receive**, which can degrade or reflect outgoing transmissions. It now follows the same switching sequence the phone-facing upstream firmware uses for this hardware.
- **The nRF52840 board ran its radio timing off an imprecise internal oscillator instead of the board's onboard precision crystal**, which this hardware has and upstream's own firmware uses. This affects Bluetooth connection stability and, more subtly, radio timing overall.
- **A watchdog reset could interrupt the nRF52840 board's own shutdown sequence**, causing it to reboot moments after powering down. The watchdog now pauses instead of continuing to run through the shutdown path.
- **The nRF52840 board's charge current was left at its power-on default (roughly half of the intended rate) instead of being explicitly configured**, extending charge time noticeably.
- **A single noisy battery reading on the nRF52840 board could trigger an unwarranted automatic shutdown.** Battery level is now averaged and smoothed the same way the ESP32 board already does, filtering out momentary dips.
- **A stale flag from a prior sleep cycle on the nRF52840 board could, in rare cases, cause the device to boot into its recovery bootloader instead of the firmware after waking.** That flag is now explicitly cleared before each shutdown.
- **The ESP32 board's antenna-boost power rail was left disabled at boot and was actually enabled (not disabled) when entering deep sleep** — both the opposite of the intended behavior, and the opposite of each other. Boot now enables it and sleep now disables it, matching the phone-facing upstream firmware for this hardware. This also resolves an open question about whether cutting this rail could interfere with waking on an incoming radio packet during deep sleep — it does not, since this rail doesn't power the radio itself.
- **The ESP32 board's radio chip-select line was left unprotected during deep sleep**, letting it float and risk misinterpreting electrical noise as real commands. It's now held in a defined state for the duration of sleep, matching upstream.
- **The ESP32 board's watchdog timeout was far shorter than upstream's**, and an admin-requested shutdown delay could exceed it entirely, causing an unwanted reset instead of a clean shutdown. The timeout now matches upstream, and the shutdown wait keeps the watchdog fed throughout.
- **The ESP32 board's battery ADC used a wider input range than this hardware's voltage divider calls for**, reducing effective reading resolution. It now uses the same setting upstream's firmware selected specifically for this board.

### Changed
- **The watchdog task's feed/inactivity/shutdown logic, the battery-voltage-to-percentage conversion, the BLE advertised device name, and the SX1262 chip configuration are now each defined once**, shared by both boards instead of duplicated. Reduces the surface area where the two boards' behavior could silently drift apart, and the voltage-to-percentage conversion is now covered by host-side tests. No behavior change on either board.

## 2026-08-25

### Fixed
- **A saved region and modem preset combination that upstream's own frequency plan can produce would panic the LoRa task on boot** instead of starting the radio. Some presets use a 250 or 500 kHz bandwidth, and lora-phy refuses those below 400 MHz, which the amateur-radio ITU regions fall under. The radio now falls back to Meshtastic's own LongFast default and logs the rejected combination instead of crashing.
- **Setting a channel PSK with an invalid length over admin silently produced a valid-length-but-wrong encryption key** instead of being rejected — the phone app could set a PSK of, say, 20 bytes, which got truncated to 16 with no error, quietly breaking that channel's encryption. PSK length is now validated against Meshtastic's four valid lengths (0, 1, 16, 32 bytes) before it's applied.
- **A failed flash write while saving device config, a BLE bond, or the NodeDB snapshot was indistinguishable from success** — the failure was logged inside the storage adapter but never reached the caller, so a debounced NodeDB flush that failed would still mark the in-memory copy as saved and never retry. Save failures are now surfaced to every caller and logged again with context; the NodeDB flush only clears its dirty flag on an actual successful write.
- **A failed first-boot save of the device's PKC keypair would silently regenerate a new identity on every reboot**, breaking every peer's ability to decrypt direct messages to this node with no visible symptom beyond "DMs stopped working." This now fails loudly at boot instead.

### Added
- **LoRa radio support on the nRF52840 board.** The Wio-SX1262's radio task now brings the SX1262 up, writes the Meshtastic sync word, and enters continuous RX/TX — the same behaviour as the ESP32 board, adapted for this module's extra RF-switch enable pin. The BLE GATT server, flash storage and the mesh orchestrator itself aren't wired up yet, so this board still can't join a mesh end-to-end; it currently just listens.
- **BLE GATT server on the nRF52840 board.** Advertising, pairing, bonding, and the Meshtastic ToRadio/FromRadio/FromNum characteristics now work the same as the ESP32 board, backed by Nordic's SoftDevice Controller instead of `esp-radio`. Flash storage isn't wired up yet, so bonds aren't persisted across reboots, and there's no mesh orchestrator yet to actually answer a config exchange or move packets between BLE and LoRa.
- **Flash storage on the nRF52840 board.** Device config, the BLE bond, buffered messages, the NodeDB snapshot and the PKC keypair now persist across reboots, matching the ESP32 board's flash layout at a different base offset. There's still no mesh orchestrator wired up, so this board can't join a mesh end-to-end yet.

### Changed
- **The firmware is now split into a hardware-agnostic core and a per-board binary**, so a second board can be added without touching the protocol. Everything that isn't chip-specific — the mesh protocol, routing, crypto and persistence — is shared; each board supplies its own radio, BLE, flash, battery and watchdog support. Boards are built and released independently, since they need different compilers. Behaviour on the Heltec ESP32-S3 is unchanged.
- **Reboots and random-number generation are now board-supplied** rather than assuming an ESP32. This covers the reboot an admin message triggers, the jitter before relaying a packet, and the nonce protecting encrypted direct messages.
- **The BLE task's device-name buffer no longer relies on a mutable static** written once at task start and read once later with no compiler-checked ordering between the two. It's built into a local buffer and handed out through a `StaticCell` instead, which the compiler can actually verify is initialized before it's read.
- **The LoRa radio's modem-config mapping and its TX/RX/CAD/channel-utilization event loop are now shared between both boards** instead of duplicated — porting the nRF52 radio task copied nearly all of the ESP32 one verbatim, since neither ever touched a board-specific type. Each board now does only its own SPI/GPIO setup and hands the initialized radio to the shared logic. No behaviour change on either board.
- **The NVS record layouts (device config, BLE bond header, PKC keypair, message-ring framing) are now defined once**, shared by every board's storage adapter, ahead of the nRF52 board getting flash storage of its own — a future field no longer needs editing in two places. Each board's adapter still does its own flash I/O, since the ESP32 and nRF52 flash drivers are sync and async respectively and can't share that part. No behaviour change on the ESP32 board.
- **The storage port traits are now async**, ahead of the nRF52 board getting flash storage of its own — its flash driver can only write and erase asynchronously, since flash access is arbitrated against radio timeslots. The ESP32's storage adapter is unaffected in behaviour; its implementation simply never yields.

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
