//! Meshtastic BLE GATT server task for the nRF52840 board.
//!
//! Structurally mirrors the ESP32 board's `ble_task` — same GATT service
//! definition, advertising loop, pairing, bond serialization and event loop —
//! but this one **cannot** be shared via a `meshtastenstein-core` module the
//! way `lora_task_body` is. `nrf-sdc`'s `SoftdeviceController` needs
//! `bt-hci 0.10`, while the ESP32's `esp-radio` controller is hard-pinned to
//! `bt-hci ^0.8.0` (checked directly against `esp-radio`'s manifest, not just
//! inherited from an old pin — no published `esp-radio` version, including
//! the 1.0 beta, has moved off it). `bt-hci` defines the `Controller` trait
//! trouble-host is built on, so the two boards are stuck on incompatible
//! trouble-host majors (0.6 git rev vs. 0.8.0), and the API shape genuinely
//! changed between them — e.g. `HostResources` dropped its controller type
//! parameter. A shared core module would have to pick one `bt-hci` major and
//! break the other board. Don't re-attempt this extraction without checking
//! whether `esp-radio` has moved first.
//!
//! One real structural difference from the ESP32 version: `SoftdeviceController`
//! implements `bt_hci::controller::Controller` directly, so it's passed
//! straight to `trouble_host::new()` — no `ExternalController` wrapper needed
//! (that type exists for byte-stream HCI transports like the ESP32's
//! `BleConnector`, which `SoftdeviceController` isn't).

extern crate alloc;
use alloc::boxed::Box;
use embassy_futures::select::{Either3, select3};
use embassy_time::{Duration, Timer};
use heapless::Vec;
use log::{debug, error, info, warn};
use meshtastenstein_core::{
    constants::*,
    domain::persistence::{self, BOND_SIZE},
    inter_task::channels::{Channels, FromRadioMessage, MeshEvent},
    ports::Reboot,
};

use crate::adapters::nrf_reboot_adapter::NrfRebootAdapter;
use nrf_sdc::SoftdeviceController;
use trouble_host::{
    Address, Identity, IoCapabilities,
    advertise::AdvertisementParameters,
    att::AttRsp,
    connection::SecurityLevel,
    gatt::{GattConnection, GattConnectionEvent, GattEvent},
    prelude::*,
};

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 1;

/// GATT Server for Meshtastic BLE service + standard Battery Service
#[gatt_server]
struct Server {
    meshtastic_service: MeshtasticService,
    battery_service: BatteryService,
}

/// Standard BLE Battery Service (UUID 0x180F)
#[gatt_service(uuid = "180f")]
struct BatteryService {
    /// Battery Level (UUID 0x2A19): single byte 0-100%
    #[characteristic(uuid = "2a19", read, notify, value = [0u8; 1])]
    battery_level: [u8; 1],
}

/// Meshtastic BLE service
#[gatt_service(uuid = "6ba1b218-15a8-461f-9fa8-5dcae273eafd")]
struct MeshtasticService {
    /// ToRadio: phone writes mesh packets here
    #[characteristic(uuid = "f75c76d2-129e-4dad-a1dd-7866124401e7", write, write_without_response, value = [0u8; 512])]
    to_radio: [u8; 512],
    /// FromRadio: phone reads mesh packets from here
    #[characteristic(uuid = "2c55e69e-4993-11ed-b878-0242ac120002", read, value = [0u8; 512])]
    from_radio: [u8; 512],
    /// FromNum: notification counter to trigger phone reads
    #[characteristic(uuid = "ed9da18c-a800-4f66-a670-aa7547e34453", read, notify, value = [0u8; 4])]
    from_num: [u8; 4],
}

/// Serialize BondInformation to 48-byte flash-storable blob:
///   [0..4]  magic, [4] version, [5..11] bd_addr bytes, [11] has_irk,
///   [12..28] irk (or zeros), [28..44] ltk, [44] security_level, [45] is_bonded
///
/// The magic/version header is shared with every board via
/// `persistence::init_bond_header` — only the trouble-host-specific fields
/// after it are written here, since `BondInformation`'s shape depends on
/// which trouble-host major the board is pinned to.
fn serialize_bond(info: &BondInformation) -> [u8; BOND_SIZE] {
    let mut b = [0u8; BOND_SIZE];
    persistence::init_bond_header(&mut b);
    b[5..11].copy_from_slice(info.identity.addr.addr.raw());
    if let Some(irk) = info.identity.irk {
        b[11] = 1;
        b[12..28].copy_from_slice(&irk.to_le_bytes());
    }
    b[28..44].copy_from_slice(&info.ltk.0.to_le_bytes());
    b[44] = match info.security_level {
        SecurityLevel::NoEncryption => 0,
        SecurityLevel::Encrypted => 1,
        SecurityLevel::EncryptedAuthenticated => 2,
    };
    b[45] = info.is_bonded as u8;
    b
}

/// Deserialize a bond blob; returns None if magic/version mismatch.
fn deserialize_bond(b: &[u8; BOND_SIZE]) -> Option<BondInformation> {
    if !persistence::bond_magic_valid(b) {
        return None;
    }
    // Phones always bond with a random static address.
    let addr = Address::random([b[5], b[6], b[7], b[8], b[9], b[10]]);
    let irk = if b[11] != 0 {
        let bytes: [u8; 16] = b[12..28].try_into().ok()?;
        IdentityResolvingKey::from_le_bytes(bytes)
    } else {
        None
    };
    let ltk = LongTermKey(u128::from_le_bytes(b[28..44].try_into().ok()?));
    let security_level = match b[44] {
        1 => SecurityLevel::Encrypted,
        2 => SecurityLevel::EncryptedAuthenticated,
        _ => SecurityLevel::NoEncryption,
    };
    Some(BondInformation {
        ltk,
        identity: Identity { addr, irk },
        security_level,
        is_bonded: b[45] != 0,
    })
}

#[embassy_executor::task]
pub async fn ble_task(
    controller: SoftdeviceController<'static>,
    channels: &'static Channels,
    initial_bond: Option<[u8; BOND_SIZE]>,
    mac: [u8; 6],
    device_name: &'static str,
) {
    info!("[BLE] Starting Meshtastic BLE task...");
    let reboot = NrfRebootAdapter;

    // Derive BLE address from MAC: use random static format (top 2 bits = 0b11)
    let address = Address::random([mac[5], mac[4], mac[3], mac[2], mac[1], mac[0] | 0xC0]);

    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
        HostResources::new();
    let stack = trouble_host::new(controller, &mut resources)
        .set_random_address(address)
        .set_io_capabilities(IoCapabilities::DisplayOnly)
        .build();

    // Restore persisted bond from NVS so the phone can reconnect after reboot without re-pairing.
    if let Some(ref bytes) = initial_bond {
        match deserialize_bond(bytes) {
            Some(bond) => {
                if let Err(e) = stack.add_bond_information(bond) {
                    warn!("[BLE] Failed to restore bond: {:?}", e);
                } else {
                    info!("[BLE] Restored bond from NVS");
                }
            }
            None => warn!("[BLE] Stored bond corrupt, ignoring"),
        }
    }

    let runner = stack.runner();
    let peripheral = stack.peripheral();

    info!("[BLE] Device name: '{}'", device_name);

    let server = match Server::new_with_config(GapConfig::Peripheral(PeripheralConfig {
        name: device_name,
        appearance: &appearance::power_device::GENERIC_POWER_DEVICE,
    })) {
        Ok(s) => s,
        Err(e) => {
            error!("[BLE] FATAL: Failed to create GATT server: {:?}", e);
            return;
        }
    };

    // Meshtastic service UUID (6ba1b218-15a8-461f-9fa8-5dcae273eafd) in little-endian
    const MESHTASTIC_SERVICE_UUID_LE: [u8; 16] = [
        0xfd, 0xea, 0x73, 0xe2, 0xca, 0x5d, 0xa8, 0x9f, 0x1f, 0x46, 0xa8, 0x15, 0x18, 0xb2, 0xa1,
        0x6b,
    ];

    // Advertising data: flags + service UUID (name goes in scan response to save space)
    let mut adv_data = [0; 31];
    let adv_data_len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::CompleteServiceUuids128(&[MESHTASTIC_SERVICE_UUID_LE]),
        ],
        &mut adv_data[..],
    )
    .unwrap();

    // Scan response: device name
    let mut scan_data = [0; 31];
    let scan_data_len = AdStructure::encode_slice(
        &[AdStructure::CompleteLocalName(device_name.as_bytes())],
        &mut scan_data[..],
    )
    .unwrap();

    embassy_futures::join::join(
        async {
            let mut runner = runner;
            if let Err(e) = runner.run().await {
                // trouble-host runner should never return under normal operation.
                // InvalidState can occur when the phone reconnects during an in-flight
                // watchdog-initiated disconnect (race between HCI disconnect completion
                // and the new connection request). The BLE hardware state is unknown;
                // a software reset is the only safe recovery.
                error!("[BLE] BLE host runner failed: {:?} — rebooting", e);
                // Short yield: lets gatt_events_loop process PairingFailed and
                // send BondClear to the mesh orchestrator before the reset.
                // Must be well under the 500ms watchdog grace period.
                Timer::after(Duration::from_millis(50)).await;
                reboot.reboot();
            }
        },
        advertising_loop(
            &stack,
            peripheral,
            &server,
            &adv_data[..adv_data_len],
            &scan_data[..scan_data_len],
            channels,
            &reboot,
        ),
    )
    .await;
}

async fn advertising_loop(
    stack: &trouble_host::Stack<'_, SoftdeviceController<'static>, DefaultPacketPool>,
    mut peripheral: Peripheral<'_, SoftdeviceController<'static>, DefaultPacketPool>,
    server: &Server<'_>,
    adv_data: &[u8],
    scan_data: &[u8],
    channels: &'static Channels,
    reboot: &impl Reboot,
) {
    let mut from_num: u32 = 0;

    loop {
        info!("[BLE] Advertising...");

        let adv_params = AdvertisementParameters {
            interval_min: Duration::from_millis(BLE_ADV_INTERVAL_MIN_MS),
            interval_max: Duration::from_millis(BLE_ADV_INTERVAL_MAX_MS),
            ..Default::default()
        };

        let acceptor = match peripheral
            .advertise(
                &adv_params,
                Advertisement::ConnectableScannableUndirected {
                    adv_data,
                    scan_data,
                },
            )
            .await
        {
            Ok(a) => a,
            Err(e) => {
                error!("[BLE] Advertising failed: {:?}", e);
                Timer::after(Duration::from_secs(1)).await;
                continue;
            }
        };

        let conn = match acceptor.accept().await {
            Ok(c) => c,
            Err(e) => {
                error!("[BLE] Connection failed: {:?}", e);
                continue;
            }
        };

        // Enable bonding so the security manager stores the LTK in RAM.
        // Bond is also persisted to NVS (via bond_save channel) for cross-reboot reconnect.
        if let Err(e) = conn.set_bondable(true) {
            warn!("[BLE] set_bondable(true) failed: {:?}", e);
        }

        let conn = match conn.with_attribute_server(server) {
            Ok(c) => c,
            Err(e) => {
                error!("[BLE] GATT attach failed: {:?}", e);
                continue;
            }
        };

        info!("[BLE] Connected!");
        let _ = channels.mesh_in.try_send(MeshEvent::BleConnected);

        // Request a fast connection interval for the initial config-exchange burst.
        // Matches upstream's onConnect updateConnParams(6, 12, 0, 200): interval
        // 7.5-15ms (units of 1.25ms), no slave latency, 2s supervision timeout.
        // Best-effort — some phones/OSes ignore or reject peripheral-initiated
        // requests, so a failure here is not fatal to the connection.
        let fast_params = trouble_host::connection::RequestedConnParams {
            min_connection_interval: Duration::from_micros(7_500),
            max_connection_interval: Duration::from_micros(15_000),
            max_latency: 0,
            min_event_length: Duration::from_micros(0),
            max_event_length: Duration::from_micros(0),
            supervision_timeout: Duration::from_secs(2),
        };
        if let Err(e) = conn
            .raw()
            .update_connection_params(stack, &fast_params)
            .await
        {
            debug!("[BLE] Connection parameter update request failed: {:?}", e);
        }

        let mut bond_clear_pending = false;
        gatt_events_loop(
            server,
            &conn,
            channels,
            &mut from_num,
            &mut bond_clear_pending,
        )
        .await;
        if bond_clear_pending {
            // NVS bond was cleared (PairingFailed); reboot so the BLE stack reloads
            // with no bond and the phone can pair fresh.
            warn!("[BLE] Bond cleared after pairing failure — rebooting to pair fresh");
            embassy_time::Timer::after(embassy_time::Duration::from_millis(50)).await;
            reboot.reboot();
        }

        let _ = channels.mesh_in.try_send(MeshEvent::BleDisconnected);
        info!("[BLE] Disconnected");
    }
}

async fn gatt_events_loop(
    server: &Server<'_>,
    conn: &GattConnection<'_, '_, DefaultPacketPool>,
    channels: &'static Channels,
    from_num: &mut u32,
    bond_clear_pending: &mut bool,
) {
    let tx_to_ble = channels.ble_tx.receiver();
    let disconnect_cmd = channels.disconn_cmd.receiver();
    let radio_stats = &channels.radio_stats;
    let bat_level = &channels.bat_level;

    let mut notifications_enabled = false;
    // Track whether from_radio has valid data; false = send 0-byte "end of queue" response
    let mut from_radio_has_data = false;
    // Buffer holding the current FromRadio packet (exact bytes, no zero padding).
    // Must stay in sync with the `from_radio` characteristic's declared size
    // (512, in the `MeshtasticService` gatt_service definition above) — that's
    // the largest a FromRadio payload can ever be, independent of the
    // negotiated MTU (Android's 508 vs this buffer's 512 is exactly why every
    // read replies with `[..from_radio_len]`, never the full buffer).
    const FROM_RADIO_CHAR_SIZE: usize = 512;
    let mut from_radio_buf = [0u8; FROM_RADIO_CHAR_SIZE];
    let mut from_radio_len = 0usize;

    loop {
        // Only pull the next message when the phone has read the current one.
        // If from_radio_has_data=true the previous packet is still waiting to be read —
        // pulling another message would overwrite from_radio_buf and silently drop it.
        let tx_fut = async {
            if notifications_enabled && !from_radio_has_data {
                tx_to_ble.receive().await
            } else {
                core::future::pending::<FromRadioMessage>().await
            }
        };

        match select3(
            radio_stats.wait(),
            bat_level.wait(),
            select3(conn.next(), tx_fut, disconnect_cmd.receive()),
        )
        .await
        {
            Either3::First(_) => {
                // Radio stats update - could notify from_num
            }
            Either3::Second((level, _voltage_mv)) => {
                // Update Battery Level characteristic (0x2A19) and notify
                if let Err(e) = server
                    .battery_service
                    .battery_level
                    .notify(conn, &[level], false)
                    .await
                {
                    debug!("[BLE] Battery level notify failed: {:?}", e);
                }
            }
            Either3::Third(Either3::First(event)) => match event {
                GattConnectionEvent::Disconnected { reason } => {
                    info!("[BLE] Disconnected: {:?}", reason);
                    break;
                }
                GattConnectionEvent::PassKeyDisplay(key) => {
                    info!("[BLE] *** Pairing PIN: {:06} ***", key.value());
                }
                GattConnectionEvent::PairingComplete {
                    security_level,
                    bond,
                } => {
                    info!(
                        "[BLE] Pairing complete, security level: {:?}",
                        security_level
                    );
                    if let Some(info) = bond {
                        let bytes = serialize_bond(&info);
                        if channels
                            .mesh_in
                            .try_send(MeshEvent::BondSave(Box::new(bytes)))
                            .is_err()
                        {
                            warn!("[BLE] bond_save: mesh_in full, bond not persisted");
                        }
                    }
                }
                GattConnectionEvent::PairingFailed(reason) => {
                    warn!("[BLE] Pairing failed: {:?}", reason);
                    // Phone likely cleared its bond data. Signal the outer loop to
                    // remove the stale bond from the in-RAM stack (stack not in scope
                    // here) and erase NVS so the next reboot pairs fresh.
                    *bond_clear_pending = true;
                    let _ = channels.mesh_in.try_send(MeshEvent::BondClear);
                }
                GattConnectionEvent::Gatt { event } => match event {
                    GattEvent::Write(write_event) => {
                        let handle = write_event.handle();
                        let is_to_radio = handle == server.meshtastic_service.to_radio.handle;

                        // Extract what we need from the write payload before accepting.
                        let (is_cccd_enable, ble_rx_msg) =
                            write_event.with_data(|_offset, data| {
                                let cccd = !is_to_radio && data == [0x01, 0x00];
                                let rx = if is_to_radio {
                                    debug!("[BLE] ToRadio write: {} bytes", data.len());
                                    let mut msg_data: Vec<u8, 512> = Vec::new();
                                    msg_data.extend_from_slice(data).ok();
                                    Some(Box::new(msg_data))
                                } else {
                                    None
                                };
                                (cccd, rx)
                            });

                        if let Some(msg_data) = ble_rx_msg
                            && channels
                                .mesh_in
                                .try_send(MeshEvent::BleRx(msg_data))
                                .is_err()
                        {
                            error!("[BLE] ToRadio: mesh_in full, DROPPED!");
                        }

                        if let Err(e) = write_event.accept().map(|r| r.send()) {
                            warn!("[BLE] Write accept failed: {:?}", e);
                        }

                        if is_cccd_enable {
                            info!("[BLE] Notifications enabled");
                            notifications_enabled = true;
                        }
                    }
                    GattEvent::Read(read_event) => {
                        let handle = read_event.handle();
                        debug!("[BLE] Read request: handle={}", handle);

                        if handle == server.meshtastic_service.from_radio.handle {
                            if from_radio_has_data {
                                // Reply with exact packet bytes — no zero-padding — avoids
                                // protobuf parse errors when MTU < 512 (e.g. Android MTU 508).
                                let payload = read_event.into_payload();
                                if let Err(e) = payload
                                    .reply(AttRsp::Read {
                                        data: &from_radio_buf[..from_radio_len],
                                    })
                                    .await
                                {
                                    warn!("[BLE] FromRadio read reply failed: {:?}", e);
                                }
                                from_radio_has_data = false;
                            } else {
                                // End-of-queue: send 0-byte ATT read response
                                debug!("[BLE] FromRadio empty — sending 0-byte end-of-queue");
                                let payload = read_event.into_payload();
                                if let Err(e) = payload.reply(AttRsp::Read { data: &[] }).await {
                                    warn!("[BLE] FromRadio empty reply failed: {:?}", e);
                                }
                            }
                        } else if let Err(e) = read_event.accept().map(|r| r.send()) {
                            warn!("[BLE] Read accept failed: {:?}", e);
                        }
                    }
                    GattEvent::NotAllowed(not_allowed) => {
                        warn!(
                            "[BLE] GATT operation not allowed on handle {}",
                            not_allowed.handle()
                        );
                        if let Err(e) = not_allowed.accept().map(|r| r.send()) {
                            warn!("[BLE] NotAllowed accept failed: {:?}", e);
                        }
                    }
                    GattEvent::Other(other_event) => {
                        if let Err(e) = other_event.accept().map(|r| r.send()) {
                            warn!("[BLE] Other GATT event accept failed: {:?}", e);
                        }
                    }
                },
                _ => {}
            },
            Either3::Third(Either3::Second(msg)) => {
                // FromRadio message to send to phone
                from_radio_len = msg.data.len().min(512);
                from_radio_buf[..from_radio_len].copy_from_slice(&msg.data[..from_radio_len]);
                from_radio_has_data = true;
                debug!("[BLE] FromRadio: {} bytes queued", from_radio_len);

                // Set FromNum to the packet's from_radio_id so the phone knows which
                // packet just arrived (Meshtastic spec: FromNum = id of last FromRadio)
                *from_num = msg.id;
                let num_bytes = from_num.to_le_bytes();
                if let Err(e) = server
                    .meshtastic_service
                    .from_num
                    .notify(conn, &num_bytes, false)
                    .await
                {
                    debug!("[BLE] FromNum notify failed: {:?}", e);
                }
            }
            Either3::Third(Either3::Third(_)) => {
                warn!("[BLE] Disconnect command from watchdog");
                break;
            }
        }
    }
}
