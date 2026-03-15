# Meshtastenstein

Meshtastic protocol firmware in Rust for the **Heltec WiFi LoRa 32 V3** (ESP32-S3 + SX1262).

This is a from-scratch implementation of the Meshtastic mesh networking protocol stack — radio, BLE, crypto, node management, and config persistence — written entirely in `no_std` Rust using the Embassy async executor.

---

## Hardware Target

| Component | Details |
|-----------|---------|
| MCU | ESP32-S3 (dual-core Xtensa LX7, 512 KB DRAM) |
| Radio | SX1262 LoRa transceiver |
| Board | Heltec WiFi LoRa 32 V3 |
| Toolchain | Xtensa ESP (`esp` channel via `rust-toolchain.toml`) |

### Pin mapping

| Signal | GPIO |
|--------|------|
| LoRa SPI SCK | 9 |
| LoRa SPI MISO | 11 |
| LoRa SPI MOSI | 10 |
| LoRa CS | 8 |
| LoRa RESET | 12 |
| LoRa DIO1 | 14 |
| LoRa BUSY | 13 |
| LED | 35 |
| Battery ADC | GPIO1 (ADC1) |
| Battery ADC ctrl | GPIO37 |

---

## Features

- **Meshtastic BLE API** — full GATT service (ToRadio / FromRadio / FromNum), MTU-correct read replies, notifications, secure pairing with PIN display, bond persistence across reboots
- **LoRa mesh** — Meshtastic packet framing (16-byte OTA header), sync word 0x2B, preamble 16 symbols, AES-128-CTR encryption, CRC, configurable modem preset and region
- **Config exchange** — complete phone app handshake: MyNodeInfo + own NodeInfo + DeviceMetadata + 8 channels + all Config types (Device / Position / Power / Network / Display / LoRa / Bluetooth / Security / Sessionkey) + all 13 ModuleConfig types + NodeDB + ConfigCompleteId
- **Admin messages** — GetOwner / SetOwner, GetConfig / SetConfig (LoRa + Device), GetChannel / SetChannel, BeginEditSettings / CommitEditSettings, **RebootSeconds** (actual software reset via `esp_hal::system::software_reset`)
- **NodeDB** — up to 64 nodes, stale eviction (2 h), rejects reserved node numbers, synced to phone in config exchange
- **Duplicate detection** — 64-entry ring buffer with 1-hour timestamp-based eviction
- **want_ack retransmission** — 3 retries × 5 s timeout, routing ACK clears entry
- **NVS persistence** — SavedConfig (names, region, modem preset, role, 8 channels) + message ring buffer + BLE bond stored in ESP-IDF NVS flash partition
- **Store-and-forward** — TEXT_MESSAGE frames buffered in NVS when BLE disconnected; replayed after next config exchange
- **Battery monitoring** — ADC sampling with voltage-divider compensation, telemetry sent as TELEMETRY_APP FromRadio
- **NodeInfo broadcast** — 5 s after boot, then every 15 min; responds to want_response NodeInfo requests
- **Position relay** — phone's POSITION_APP payload cached and re-broadcast to mesh every 30 min
- **Deep sleep** — inactivity watchdog (5 min), DIO1/button wakeup source
- **LED heartbeat** — 2 s pulse pattern, single blink on LoRa RX

---

## Architecture

```
src/
├── bin/main.rs              Entry point, peripheral init, task spawning
├── constants.rs             All compile-time constants (frequencies, timings, crypto)
├── lib.rs                   Crate root, re-exports modules
│
├── mesh/                    Protocol domain (no hardware dependencies)
│   ├── packet.rs            RadioFrame, PacketHeader, OTA framing
│   ├── crypto.rs            AES-128-CTR (RustCrypto), nonce construction
│   ├── router.rs            Duplicate detection, rebroadcast scheduling
│   ├── node_db.rs           NodeDB (64-entry, stale eviction)
│   ├── channels.rs          ChannelSet, ChannelConfig, channel hash
│   ├── device.rs            DeviceState (node num, names, role, preset, region)
│   ├── portnum_handler.rs   Dispatch by portnum (Text, NodeInfo, Position, ...)
│   └── radio_config.rs      Region + ModemPreset enums, frequency calculation
│
├── tasks/                   Embassy async tasks
│   ├── mesh_task.rs         MeshOrchestrator — central event loop, admin handler
│   ├── lora_task.rs         SX1262 driver, CAD TX, continuous RX, jitter
│   ├── ble_task.rs          GATT server, pairing, bond, FromRadio delivery
│   ├── battery_task.rs      ADC sampling, telemetry
│   ├── led_task.rs          LED blink patterns
│   └── watchdog_task.rs     HW watchdog feed, inactivity → deep sleep
│
├── adapters/
│   ├── nvs_storage_adapter.rs   Flash read/write (SavedConfig, bond, message ring)
│   └── deep_sleep_adapter.rs    ESP32 deep sleep entry
│
├── drivers/
│   └── sx1262_direct.rs    Direct SPI register writes for sync word
│
├── inter_task/
│   └── channels.rs          All Embassy Channel / Signal definitions
│
├── ports/
│   ├── storage.rs           Storage trait (add/peek/pop/clear)
│   └── sleep.rs             Sleep trait
│
└── proto/
    ├── meshtastic.rs        prost-generated Meshtastic protobuf types
    └── _.rs                 google.protobuf stubs
```

### Inter-task channel topology

```
                    ┌──────────────┐
                    │   Watchdog   │
                    └──────┬───────┘
                           │ disconn_cmd
                           ▼
┌─────────┐  ble_rx   ┌─────────────┐  lora_tx   ┌─────────┐
│   BLE   │◄─────────►│    Mesh     │◄──────────►│  LoRa   │
│  Task   │  ble_tx   │  (main)     │  lora_rx   │  Task   │
└────┬────┘           └──────┬──────┘            └─────────┘
     │ conn_state            │ led_cmd / activity
     │ bond_save             ▼
     │               ┌─────────────┐
     │               │  LED Task   │
     │               └─────────────┘
     │ bat_level (Signal)
     ▼
┌─────────┐
│ Battery │
│  Task   │
└─────────┘
```

---

## Key Protocol Details

| Parameter | Value |
|-----------|-------|
| Sync word | 0x2B (SX1262 regs 0x0740=0x24, 0x0741=0xB4) |
| Preamble | 16 symbols |
| Default preset | LongFast: SF11, BW 250 kHz, CR 4/5 |
| Default region | EU_433 — 433.875 MHz (slot 3) |
| OTA header | 16 bytes: dest u32 LE, sender u32 LE, packet_id u32 LE, flags u8, channel_index u8, next_hop u8, relay_node u8 |
| Encryption | AES-128-CTR, nonce = packet_id (u32 LE) + sender (u32 LE) + padding |
| Default PSK | `d4f1bb3a20290759f0bcffabcf4e6901` |
| BLE service UUID | `6ba1b218-15a8-461f-9fa8-5dcae273eafd` |
| ToRadio char | `f75c76d2-129e-4dad-a1dd-7866124401e7` (write) |
| FromRadio char | `2c55e69e-4993-11ed-b878-0242ac120002` (read) |
| FromNum char | `ed9da18c-a800-4f66-a670-aa7547e34453` (read + notify) |
| BLE MTU | Android negotiates 508; FromRadio replies use `AttRsp::Read { data: &buf[..len] }` (exact bytes, no zero-padding) |
| NVS layout | SavedConfig at offset 0x0000 (512 B), Bond at 0x0200 (48 B), message ring from 0x1000 |

### Region frequency table (LongFast / BW 250 kHz)

| Region | Code | Default slot | Frequency |
|--------|------|-------------|-----------|
| US | 1 | 20 | 907.125 MHz |
| EU_433 | 2 | 3 | 433.875 MHz |
| EU_868 | 3 | 0 | 869.525 MHz |
| ANZ | 6 | 20 | 917.125 MHz |

---

## Build

Requires the Xtensa ESP Rust toolchain (managed via `rust-toolchain.toml`):

```bash
# Install espup if needed
cargo install espup
espup install

# Check (no linker needed for type-checking)
cargo check

# Build + flash (requires espflash and the Xtensa toolchain active)
cargo build --release
espflash flash --monitor target/xtensa-esp32s3-none-elf/release/meshtastenstein
```

Set log level via environment variable before flashing:
```bash
RUST_LOG=debug cargo build --release
```

### Protobuf generation

Protobufs live as a git submodule at `proto/meshtastic-protobufs/`. Generated Rust types are committed to `src/proto/`. To regenerate:

```bash
git submodule update --init
cargo build  # triggers build.rs → prost-build
```

---

## Current Status

### Working
- BLE pairing (PIN display), bonding, NVS bond persistence, cross-reboot reconnect
- Full config exchange (app reaches "connected" state)
- LoRa TX from phone → mesh (admin messages, telemetry, text)
- LoRa RX → BLE forwarding
- Modem preset change via app (saves to NVS, applied on next reboot via `RebootSeconds`)
- Node identity, NodeInfo broadcast, NodeDB sync to phone
- Battery telemetry
- Deep sleep / wakeup

### Known Limitations / TODO
- M5: Rebroadcast delay uses fixed jitter rather than carrier-sense; CAD logic is basic
- No LoRa frequency change without reboot (by design — requires `RebootSeconds`)
- No unit tests
- Single region compile-time default; multi-region is runtime via NVS

---

## License

No license yet — private project.
