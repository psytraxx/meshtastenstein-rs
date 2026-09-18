//! Meshtastic BLE GATT server task
//!
//! Implements the Meshtastic BLE API with:
//! - Service UUID: 6ba1b218-15a8-461f-9fa8-5dcae273eafd
//! - ToRadio char: f75c76d2-129e-4dad-a1dd-7866124401e7 (write)
//! - FromRadio char: 2c55e69e-4993-11ed-b878-0242ac120002 (read)
//! - FromNum char: ed9da18c-a800-4f66-a670-aa7547e34453 (read+notify)

#![allow(clippy::needless_borrows_for_generic_args)]

extern crate alloc;
use alloc::boxed::Box;
use embassy_futures::select::{Either3, select3};
use embassy_time::{Duration, Timer};
use esp_radio::ble::controller::BleConnector;
use heapless::Vec;
use log::{debug, error, info, warn};
use meshtastenstein_core::{
    constants::*,
    domain::persistence::{self, BOND_SIZE},
    inter_task::channels::{Channels, FromRadioMessage, MeshEvent},
};
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
    bt_peripheral: esp_hal::peripherals::BT<'static>,
    channels: &'static Channels,
    initial_bond: Option<[u8; BOND_SIZE]>,
    mac: [u8; 6],
    device_name_str: &'static str,
) {
    info!("[BLE] Starting Meshtastic BLE task...");

    let transport = match BleConnector::new(bt_peripheral, Default::default()) {
        Ok(t) => t,
        Err(e) => {
            error!("[BLE] FATAL: Failed to create BLE connector: {:?}", e);
            return;
        }
    };

    let mut controller = Some(ExternalController::<_, BLE_HCI_CMD_SLOTS>::new(transport));
    // Derive BLE address from MAC: use random static format (top 2 bits = 0b11)
    let address = Address::random([mac[5], mac[4], mac[3], mac[2], mac[1], mac[0] | 0xC0]);

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
            AdStructure::CompleteServiceUuids128(&[MESHTASTIC_SERVICE_UUID_LE]),
        ],
        &mut adv_data[..],
    )
    .unwrap();

    // Scan response: device name
    let mut scan_data = [0; 31];
    let scan_data_len = AdStructure::encode_slice(
        &[AdStructure::CompleteLocalName(device_name_str.as_bytes())],
        &mut scan_data[..],
    )
    .unwrap();

    // The trouble-host runner can return `InvalidState` when a bonded phone
    // reconnects: its security manager rejects an SMP command that arrives
    // with no matching pairing state and propagates the error all the way out,
    // tearing down the whole host. That kills BLE for good, and rebooting the
    // node instead (what this used to do) is worse — the LESC keypair is
    // regenerated from the controller RNG on every stack startup, so a reboot
    // permanently invalidates the bond the phone just stored and leaves it in
    // an "Authentication Failure" reconnect loop.
    //
    // So rebuild the stack in place and carry the bond across the restart.
    // `bond_bytes` tracks the live bond: it starts from NVS and is updated
    // whenever a new pairing completes, so a restart re-registers whatever
    // bond is currently valid rather than a stale boot-time copy.
    let mut bond_bytes = initial_bond;

    loop {
        let Some(ctrl) = controller.take() else {
            error!("[BLE] FATAL: controller consumed, cannot restart");
            return;
        };

        let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
            HostResources::new();
        let stack = trouble_host::new(ctrl, &mut resources)
            .set_random_address(address)
            .set_io_capabilities(IoCapabilities::DisplayOnly)
            .build();

        // Restore the bond so the phone can reconnect without re-pairing —
        // after a reboot, and after each of these in-place restarts.
        if let Some(ref bytes) = bond_bytes {
            match deserialize_bond(bytes) {
                Some(bond) => {
                    if let Err(e) = stack.add_bond_information(bond) {
                        warn!("[BLE] Failed to restore bond: {:?}", e);
                    } else {
                        info!("[BLE] Restored bond");
                    }
                }
                None => warn!("[BLE] Stored bond corrupt, ignoring"),
            }
        }

        let mut runner = stack.runner();
        let peripheral = stack.peripheral();

        embassy_futures::select::select(
            async {
                match runner.run().await {
                    Ok(()) => warn!("[BLE] host runner exited cleanly (unexpected)"),
                    Err(e) => error!("[BLE] host runner failed: {:?} — restarting BLE", e),
                }
            },
            advertising_loop(
                &stack,
                peripheral,
                &server,
                &adv_data[..adv_data_len],
                &scan_data[..scan_data_len],
                channels,
                &mut bond_bytes,
            ),
        )
        .await;

        // Make sure the mesh side doesn't think a phone is still attached
        // across the restart.
        let _ = channels.mesh_in.try_send(MeshEvent::BleDisconnected);

        // Brief pause so a persistently failing controller can't spin this
        // loop hot; the phone's own reconnect backoff is far longer anyway.
        Timer::after(Duration::from_millis(500)).await;
        info!("[BLE] Restarting BLE stack...");

        // Drop the stack, which drops the ExternalController and with it the
        // BleConnector. `BleConnector`'s Drop calls esp-radio's `ble_deinit`
        // and releases its PHY/radio guards, so the controller is fully torn
        // down here rather than left half-initialized.
        drop(stack);

        // SAFETY: the only `BT` handle in existence was owned by the
        // BleConnector inside the stack just dropped above, so nothing aliases
        // it at this point. `BleConnector::new` re-runs `ble_init`, which is
        // what makes the re-`steal` meaningful rather than a way to get two
        // live handles.
        let bt = unsafe { esp_hal::peripherals::BT::steal() };
        controller = Some(ExternalController::<_, BLE_HCI_CMD_SLOTS>::new(
            match BleConnector::new(bt, Default::default()) {
                Ok(t) => t,
                Err(e) => {
                    error!("[BLE] FATAL: failed to recreate BLE connector: {:?}", e);
                    return;
                }
            },
        ));
    }
}

async fn advertising_loop(
    stack: &trouble_host::Stack<
        '_,
        ExternalController<BleConnector<'static>, BLE_HCI_CMD_SLOTS>,
        DefaultPacketPool,
    >,
    mut peripheral: Peripheral<
        '_,
        ExternalController<BleConnector<'static>, BLE_HCI_CMD_SLOTS>,
        DefaultPacketPool,
    >,
    server: &Server<'_>,
    adv_data: &[u8],
    scan_data: &[u8],
    channels: &'static Channels,
    bond_bytes: &mut Option<[u8; BOND_SIZE]>,
) {
    let mut from_num: u32 = 0;
    let tx_to_ble_drain = channels.ble_tx.receiver();

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

        // Drop anything left over from a previous connection before accepting
        // a new one. A phone that disconnects mid-config-exchange leaves the
        // rest of that exchange queued; serving those stale packets to the
        // next connection makes the app see a ConfigCompleteId for a
        // want_config_id it has already moved on from, and it waits forever
        // for a completion that already went by. The queue is only meaningful
        // for the connection it was produced for.
        let mut drained = 0usize;
        while tx_to_ble_drain.try_receive().is_ok() {
            drained += 1;
        }
        if drained > 0 {
            info!("[BLE] Dropped {} stale queued packet(s)", drained);
        }

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
        // Captured before the event loop so a stale bond can be removed by
        // identity after the connection drops.
        let peer_identity = conn.raw().peer_identity();
        // MeshEvent::BleConnected is NOT sent here. A raw GATT connect is not
        // yet a usable link — the phone can't read/write anything meaningful
        // until encryption completes, and a connection that fails pairing and
        // drops immediately (see BondLost/PairingFailed below) never becomes
        // one. Upstream's onAuthenticationComplete gates on the equivalent
        // check (desc->sec_state.encrypted) rather than raw connect for the
        // same reason. The signal is sent from GattConnectionEvent::Encrypted
        // instead, which fires on both a fresh pairing and a bonded reconnect.

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
            stack,
            server,
            &conn,
            channels,
            &mut from_num,
            &mut bond_clear_pending,
            bond_bytes,
        )
        .await;
        if bond_clear_pending {
            // The peer rejected our keys, so the bond is stale on both sides.
            // Drop it from the in-RAM stack too — NVS was already cleared via
            // BondClear. Rebooting to reload the stack (what this used to do)
            // would regenerate the LESC keypair and invalidate the *next* bond
            // the phone establishes, so remove it in place instead.
            *bond_bytes = None;
            if let Err(e) = stack.remove_bond_information(peer_identity) {
                // Usually `NotFound`: trouble-host's own disconnect handling
                // already prunes a non-bonded entry, so there is nothing left
                // to remove. Expected, not a problem.
                debug!("[BLE] Stale bond not present in stack: {:?}", e);
            } else {
                info!("[BLE] Stale bond removed — next connection will pair fresh");
            }
        }

        let _ = channels.mesh_in.try_send(MeshEvent::BleDisconnected);
        info!("[BLE] Disconnected");
    }
}

async fn gatt_events_loop(
    stack: &trouble_host::Stack<
        '_,
        ExternalController<BleConnector<'static>, BLE_HCI_CMD_SLOTS>,
        DefaultPacketPool,
    >,
    server: &Server<'_>,
    conn: &GattConnection<'_, '_, DefaultPacketPool>,
    channels: &'static Channels,
    from_num: &mut u32,
    bond_clear_pending: &mut bool,
    bond_bytes: &mut Option<[u8; BOND_SIZE]>,
) {
    let tx_to_ble = channels.ble_tx.receiver();
    let disconnect_cmd = channels.disconn_cmd.receiver();
    let radio_stats = &channels.radio_stats;
    let bat_level = &channels.bat_level;

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
    // Diagnostic only: how many FromRadio packets this connection has served.
    let mut reads_served = 0u32;

    loop {
        // Only pull the next message when the phone has read the current one.
        // If from_radio_has_data=true the previous packet is still waiting to be read —
        // pulling another message would overwrite from_radio_buf and silently drop it.
        let tx_fut = async {
            if server.meshtastic_service.from_num.should_notify(conn) && !from_radio_has_data {
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
                        // Keep the in-RAM copy too, so an in-place BLE restart
                        // re-registers this bond rather than the older one that
                        // was loaded from NVS at boot.
                        *bond_bytes = Some(bytes);
                        if channels
                            .mesh_in
                            .try_send(MeshEvent::BondSave(Box::new(bytes)))
                            .is_err()
                        {
                            warn!("[BLE] bond_save: mesh_in full, bond not persisted");
                        }
                    }
                }
                GattConnectionEvent::BondLost => {
                    // The peer started a fresh pairing while we still hold a bond
                    // for it — i.e. the phone was told to forget this device. Our
                    // copy of the keys is now worthless: keeping it makes every
                    // later reconnect fail the LTK lookup and disconnect with
                    // "Authentication Failure", which no retry can recover from
                    // because that path never reaches PairingFailed. Drop our
                    // half so the pairing now in progress can replace it.
                    warn!("[BLE] Peer lost its bond — clearing ours to re-pair");
                    *bond_clear_pending = true;
                    let _ = channels.mesh_in.try_send(MeshEvent::BondClear);
                }
                GattConnectionEvent::PairingFailed(reason) => {
                    // Only a security-layer rejection means the peer actually
                    // refused our keys — that's the case where our stored bond
                    // is stale and worth dropping. Every other variant is our
                    // own stack failing (most often `InvalidState`, when the
                    // phone starts encryption while the security manager is
                    // still tearing down the previous connection). Clearing the
                    // bond on those is actively harmful: the phone keeps its
                    // half, we throw ours away, and every later reconnect fails
                    // the LTK lookup with "Authentication Failure" forever.
                    let peer_rejected = matches!(reason, trouble_host::Error::Security(_));
                    if peer_rejected {
                        warn!(
                            "[BLE] Pairing rejected by peer: {:?} — clearing bond",
                            reason
                        );
                        *bond_clear_pending = true;
                        let _ = channels.mesh_in.try_send(MeshEvent::BondClear);
                    } else {
                        warn!(
                            "[BLE] Pairing failed: {:?} — keeping bond, will retry",
                            reason
                        );
                    }
                }
                GattConnectionEvent::Encrypted { security_level, .. } => {
                    // The link is now actually usable — send BleConnected here
                    // rather than on raw GATT connect (see the comment where
                    // the connection was accepted, above). Fires both on a
                    // fresh pairing (after PairingComplete) and on a bonded
                    // reconnect that skips pairing entirely.
                    info!("[BLE] Link encrypted, security level: {:?}", security_level);
                    let _ = channels.mesh_in.try_send(MeshEvent::BleConnected);
                }
                GattConnectionEvent::RequestConnectionParams(req) => {
                    // Dropping this without a response only logs a noisy
                    // "dropped without being accepted/rejected" error inside
                    // trouble-host — it does not reject the peer's request —
                    // but there's no reason to leave it unanswered. Accept
                    // the peer's own requested parameters unconditionally;
                    // we have no competing preference to enforce here.
                    let params = req.params().clone();
                    if let Err(e) = req.accept(Some(&params), stack).await {
                        debug!("[BLE] Failed to accept connection params request: {:?}", e);
                    }
                }
                GattConnectionEvent::PassKeyConfirm(_)
                | GattConnectionEvent::PassKeyInput
                | GattConnectionEvent::OobRequest => {
                    // Only reachable via a pairing method our IoCapabilities
                    // (DisplayOnly) shouldn't select — numeric comparison,
                    // keyboard entry, or out-of-band. If one of these ever
                    // fires, the peer is negotiating a method we have no way
                    // to service, and pairing will silently stall waiting on
                    // a response we can't give. Loud rather than silent.
                    warn!("[BLE] Unsupported pairing method requested by peer — cannot proceed");
                }
                GattConnectionEvent::Gatt { event } => match event {
                    GattEvent::Write(write_event) => {
                        let handle = write_event.handle();
                        let is_to_radio = handle == server.meshtastic_service.to_radio.handle;

                        // Extract what we need from the write payload before accepting.
                        let ble_rx_msg = write_event.with_data(|_offset, data| {
                            debug!(
                                "[BLE] Write: handle={} to_radio_handle={} match={} len={}",
                                handle,
                                server.meshtastic_service.to_radio.handle,
                                is_to_radio,
                                data.len()
                            );
                            if is_to_radio {
                                let mut msg_data: Vec<u8, 512> = Vec::new();
                                msg_data.extend_from_slice(data).ok();
                                Some(Box::new(msg_data))
                            } else {
                                None
                            }
                        });

                        match write_event.accept() {
                            Ok(reply) => reply.send().await,
                            Err(e) => warn!("[BLE] Write accept failed: {:?}", e),
                        }

                        // Deliver reliably, unlike the other producers into mesh_in:
                        // this carries the phone's actual protocol request (e.g.
                        // want_config_id), which the app sends once and then waits
                        // on — there's no retry on its side if we silently drop it,
                        // unlike BleConnected/BatteryUpdate where the next update
                        // supersedes a dropped one anyway.
                        if let Some(msg_data) = ble_rx_msg {
                            channels.mesh_in.send(MeshEvent::BleRx(msg_data)).await;
                        }
                    }
                    GattEvent::Read(read_event) => {
                        let handle = read_event.handle();
                        debug!("[BLE] Read request: handle={}", handle);

                        if handle == server.meshtastic_service.from_radio.handle {
                            // The phone reads FromRadio back-to-back until it gets
                            // an empty reply, and its next read usually arrives
                            // before the select loop gets a turn to refill the
                            // buffer. `conn.next()` is polled ahead of the channel,
                            // so without this the still-queued exchange would be
                            // answered with the end-of-queue marker — the phone
                            // then stops reading mid-handshake and waits forever
                            // for a completion it has already skipped past.
                            if !from_radio_has_data && let Ok(msg) = tx_to_ble.try_receive() {
                                from_radio_len = msg.data.len().min(FROM_RADIO_CHAR_SIZE);
                                from_radio_buf[..from_radio_len]
                                    .copy_from_slice(&msg.data[..from_radio_len]);
                                from_radio_has_data = true;
                                *from_num = msg.id;
                            }

                            if from_radio_has_data {
                                reads_served += 1;
                                debug!(
                                    "[BLE] FromRadio read #{}: id={} len={}",
                                    reads_served, *from_num, from_radio_len
                                );
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
                                debug!(
                                    "[BLE] FromRadio end-of-queue after {} read(s)",
                                    reads_served
                                );
                                let payload = read_event.into_payload();
                                if let Err(e) = payload.reply(AttRsp::Read { data: &[] }).await {
                                    warn!("[BLE] FromRadio empty reply failed: {:?}", e);
                                }
                            }
                        } else {
                            match read_event.accept() {
                                Ok(reply) => reply.send().await,
                                Err(e) => warn!("[BLE] Read accept failed: {:?}", e),
                            }
                        }
                    }
                    GattEvent::NotAllowed(not_allowed) => {
                        warn!(
                            "[BLE] GATT operation not allowed on handle {}",
                            not_allowed.handle()
                        );
                        match not_allowed.accept() {
                            Ok(reply) => reply.send().await,
                            Err(e) => warn!("[BLE] NotAllowed accept failed: {:?}", e),
                        }
                    }
                    GattEvent::Other(other_event) => match other_event.accept() {
                        Ok(reply) => reply.send().await,
                        Err(e) => warn!("[BLE] Other GATT event accept failed: {:?}", e),
                    },
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
                match server
                    .meshtastic_service
                    .from_num
                    .notify(conn, &num_bytes, false)
                    .await
                {
                    Ok(()) => debug!("[BLE] FromNum notify sent: id={}", *from_num),
                    Err(e) => warn!("[BLE] FromNum notify failed: {:?}", e),
                }
            }
            Either3::Third(Either3::Third(_)) => {
                warn!("[BLE] Disconnect command from watchdog");
                break;
            }
        }
    }
}
