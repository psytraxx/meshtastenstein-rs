//! Meshtastic BLE GATT server task
//!
//! Implements the Meshtastic BLE API with:
//! - Service UUID: 6ba1b218-15a8-461f-9fa8-5dcae273eafd
//! - ToRadio char: f75c76d2-129e-4dad-a1dd-7866124401e7 (write)
//! - FromRadio char: 2c55e69e-4993-11ed-b878-0242ac120002 (read)
//! - FromNum char: ed9da18c-a800-4f66-a670-aa7547e34453 (read+notify)

use crate::constants::*;
use crate::inter_task::channels::{Channels, FromRadioMessage, ToRadioMessage};
use bt_hci::controller::ExternalController;
use bt_hci::param::BdAddr;
use embassy_futures::select::{Either3, select3};
use embassy_time::{Duration, Timer};
use esp_radio::Controller;
use esp_radio::ble::controller::BleConnector;
use log::{debug, error, info, warn};
use trouble_host::att::AttRsp;
use trouble_host::prelude::*;
use trouble_host::{
    Address, Identity, IoCapabilities,
    advertise::AdvertisementParameters,
    connection::SecurityLevel,
    gatt::{GattConnection, GattConnectionEvent, GattEvent},
};

const BOND_MAGIC: u32 = 0x424F4E44;
const BOND_VERSION: u8 = 1;
const BOND_SIZE: usize = 48;

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

static mut DEVICE_NAME_BYTES: [u8; 24] = [0u8; 24];
static mut DEVICE_NAME_LEN: usize = 0;

/// Serialize BondInformation to 48-byte flash-storable blob:
///   [0..4]  magic, [4] version, [5..11] bd_addr, [11] has_irk,
///   [12..28] irk (or zeros), [28..44] ltk, [44] security_level, [45] is_bonded
fn serialize_bond(info: &BondInformation) -> [u8; BOND_SIZE] {
    let mut b = [0u8; BOND_SIZE];
    b[0..4].copy_from_slice(&BOND_MAGIC.to_le_bytes());
    b[4] = BOND_VERSION;
    b[5..11].copy_from_slice(info.identity.bd_addr.raw());
    if let Some(irk) = info.identity.irk {
        b[11] = 1;
        b[12..28].copy_from_slice(&irk.0.to_le_bytes());
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
    let magic = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    if magic != BOND_MAGIC || b[4] != BOND_VERSION {
        return None;
    }
    let bd_addr = BdAddr::new([b[5], b[6], b[7], b[8], b[9], b[10]]);
    let irk = if b[11] != 0 {
        Some(IdentityResolvingKey(u128::from_le_bytes(
            b[12..28].try_into().ok()?,
        )))
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
        identity: Identity { bd_addr, irk },
        security_level,
        is_bonded: b[45] != 0,
    })
}

#[embassy_executor::task]
pub async fn ble_task(
    radio: &'static Controller<'static>,
    bt_peripheral: esp_hal::peripherals::BT<'static>,
    channels: &'static Channels,
    initial_bond: Option<[u8; BOND_SIZE]>,
    mac: [u8; 6],
) {
    info!("[BLE] Starting Meshtastic BLE task...");

    // Build device name: "Meshtastic_XXXX" from last 2 MAC bytes
    unsafe {
        let prefix = BLE_DEVICE_NAME_PREFIX.as_bytes();
        let mut pos = 0;
        for &b in prefix {
            DEVICE_NAME_BYTES[pos] = b;
            pos += 1;
        }
        let hex = b"0123456789ABCDEF";
        for &byte in &mac[4..6] {
            DEVICE_NAME_BYTES[pos] = hex[(byte >> 4) as usize];
            pos += 1;
            DEVICE_NAME_BYTES[pos] = hex[(byte & 0x0f) as usize];
            pos += 1;
        }
        DEVICE_NAME_LEN = pos;
    }

    let transport = match BleConnector::new(radio, bt_peripheral, Default::default()) {
        Ok(t) => t,
        Err(e) => {
            error!("[BLE] FATAL: Failed to create BLE connector: {:?}", e);
            return;
        }
    };

    let controller = ExternalController::<_, 20>::new(transport);
    // Derive BLE address from MAC: use random static format (top 2 bits = 0b11)
    let address = Address::random([mac[5], mac[4], mac[3], mac[2], mac[1], mac[0] | 0xC0]);

    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
        HostResources::new();
    let stack = trouble_host::new(controller, &mut resources)
        .set_random_address(address)
        .set_io_capabilities(IoCapabilities::DisplayOnly);

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

    let Host {
        mut peripheral,
        runner,
        ..
    } = stack.build();

    let (device_name_bytes, _) =
        unsafe { (&DEVICE_NAME_BYTES[..DEVICE_NAME_LEN], DEVICE_NAME_LEN) };
    let device_name_str = core::str::from_utf8(device_name_bytes).unwrap_or("Meshtastic");
    info!("[BLE] Device name: '{}'", device_name_str);

    let server = match Server::new_with_config(GapConfig::Peripheral(PeripheralConfig {
        name: device_name_str,
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
            AdStructure::ServiceUuids128(&[MESHTASTIC_SERVICE_UUID_LE]),
        ],
        &mut adv_data[..],
    )
    .unwrap();

    // Scan response: device name
    let mut scan_data = [0; 31];
    let scan_data_len = AdStructure::encode_slice(
        &[AdStructure::CompleteLocalName(device_name_bytes)],
        &mut scan_data[..],
    )
    .unwrap();

    embassy_futures::join::join(
        async {
            let mut runner = runner;
            runner.run().await.unwrap();
        },
        advertising_loop(
            &mut peripheral,
            &server,
            &adv_data[..adv_data_len],
            &scan_data[..scan_data_len],
            channels,
        ),
    )
    .await;
}

async fn advertising_loop(
    peripheral: &mut Peripheral<
        '_,
        ExternalController<BleConnector<'static>, 20>,
        DefaultPacketPool,
    >,
    server: &Server<'_>,
    adv_data: &[u8],
    scan_data: &[u8],
    channels: &'static Channels,
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
        let _ = channels.conn_state.sender().try_send(true);

        gatt_events_loop(server, &conn, channels, &mut from_num).await;

        let _ = channels.conn_state.sender().try_send(false);
        info!("[BLE] Disconnected");
    }
}

async fn gatt_events_loop(
    server: &Server<'_>,
    conn: &GattConnection<'_, '_, DefaultPacketPool>,
    channels: &'static Channels,
    from_num: &mut u32,
) {
    let tx_to_ble = channels.ble_tx.receiver();
    let rx_from_ble = channels.ble_rx.sender();
    let disconnect_cmd = channels.disconn_cmd.receiver();
    let radio_stats = &channels.radio_stats;
    let bond_save = channels.bond_save.sender();

    let bat_level = &channels.bat_level;

    let mut notifications_enabled = false;
    // Track whether from_radio has valid data; false = send 0-byte "end of queue" response
    let mut from_radio_has_data = false;
    // Buffer holding the current FromRadio packet (exact bytes, no zero padding)
    let mut from_radio_buf = [0u8; 512];
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
                    .notify(conn, &[level])
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
                        if bond_save.try_send(bytes).is_err() {
                            warn!("[BLE] bond_save channel full, bond not persisted");
                        }
                    }
                }
                GattConnectionEvent::PairingFailed(reason) => {
                    warn!("[BLE] Pairing failed: {:?}", reason);
                }
                GattConnectionEvent::Gatt { event } => match event {
                    GattEvent::Write(write_event) => {
                        let handle = write_event.handle();
                        let data = write_event.data();

                        let is_to_radio = handle == server.meshtastic_service.to_radio.handle;
                        let is_cccd_enable = !is_to_radio && data == [0x01, 0x00];

                        if is_to_radio {
                            debug!("[BLE] ToRadio write: {} bytes", data.len());
                            let mut msg_data = heapless::Vec::new();
                            msg_data.extend_from_slice(data).ok();
                            let msg = ToRadioMessage { data: msg_data };
                            if rx_from_ble.try_send(msg).is_err() {
                                error!("[BLE] ToRadio channel full, DROPPED!");
                            }
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
                    GattEvent::Other(other_event) => {
                        if let Err(e) = other_event.accept().map(|r| r.send()) {
                            warn!("[BLE] Other event accept failed: {:?}", e);
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
                    .notify(conn, &num_bytes)
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
