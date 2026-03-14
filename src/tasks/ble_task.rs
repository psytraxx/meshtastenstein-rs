//! Meshtastic BLE GATT server task
//!
//! Implements the Meshtastic BLE API with:
//! - Service UUID: 6ba1b218-15a8-461f-9fa8-5dcae273eafd
//! - ToRadio char: f75c76d2-129e-4dad-a1dd-7866124401e7 (write)
//! - FromRadio char: 2c55e69e-4993-11ed-b878-0242ac120002 (read)
//! - FromNum char: ed9da18c-a800-4f66-a670-aa7547e34453 (read+notify)

#![allow(clippy::too_many_arguments)]

use crate::constants::*;
use crate::inter_task::channels::{FromRadioMessage, ToRadioMessage};
use bt_hci::controller::ExternalController;
use embassy_futures::select::{Either, Either4, select, select4};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::{Receiver, Sender};
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use esp_hal::efuse::Efuse;
use esp_radio::Controller;
use esp_radio::ble::controller::BleConnector;
use log::{debug, error, info, warn};
use trouble_host::prelude::*;
use trouble_host::{
    Address,
    advertise::AdvertisementParameters,
    gatt::{GattConnection, GattConnectionEvent, GattEvent},
};

const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 1;

/// GATT Server for Meshtastic BLE service
#[gatt_server]
struct Server {
    meshtastic_service: MeshtasticService,
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

#[embassy_executor::task]
pub async fn ble_task(
    radio: &'static Controller<'static>,
    bt_peripheral: esp_hal::peripherals::BT<'static>,
    tx_to_ble: Receiver<'static, CriticalSectionRawMutex, FromRadioMessage, 10>,
    rx_from_ble: Sender<'static, CriticalSectionRawMutex, ToRadioMessage, 5>,
    battery_level: Receiver<'static, CriticalSectionRawMutex, u8, 1>,
    connection_state: Sender<'static, CriticalSectionRawMutex, bool, 1>,
    disconnect_cmd: Receiver<'static, CriticalSectionRawMutex, (), 1>,
    radio_stats: &'static Signal<CriticalSectionRawMutex, (i16, i8)>,
) {
    info!("[BLE] Starting Meshtastic BLE task...");

    let mac = Efuse::read_base_mac_address();

    // Build device name: "Meshtastic_XXXX" from last 2 MAC bytes
    unsafe {
        let prefix = BLE_DEVICE_NAME_PREFIX.as_bytes();
        let mut pos = 0;
        for &b in prefix {
            DEVICE_NAME_BYTES[pos] = b;
            pos += 1;
        }
        let hex = b"0123456789abcdef";
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
    let address = Address::random([0xff, 0x8f, 0x1a, 0x05, 0xe4, 0xff]);

    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
        HostResources::new();
    let stack = trouble_host::new(controller, &mut resources).set_random_address(address);

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

    // Meshtastic service UUID in advertising data (128-bit, little-endian)
    let mut adv_data = [0; 31];
    let adv_data_len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::CompleteLocalName(device_name_bytes),
        ],
        &mut adv_data[..],
    )
    .unwrap();

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
            tx_to_ble,
            rx_from_ble,
            battery_level,
            connection_state,
            disconnect_cmd,
            radio_stats,
        ),
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn advertising_loop(
    peripheral: &mut Peripheral<
        '_,
        ExternalController<BleConnector<'static>, 20>,
        DefaultPacketPool,
    >,
    server: &Server<'_>,
    adv_data: &[u8],
    scan_data: &[u8],
    tx_to_ble: Receiver<'static, CriticalSectionRawMutex, FromRadioMessage, 10>,
    rx_from_ble: Sender<'static, CriticalSectionRawMutex, ToRadioMessage, 5>,
    battery_level: Receiver<'static, CriticalSectionRawMutex, u8, 1>,
    connection_state: Sender<'static, CriticalSectionRawMutex, bool, 1>,
    disconnect_cmd: Receiver<'static, CriticalSectionRawMutex, (), 1>,
    radio_stats: &'static Signal<CriticalSectionRawMutex, (i16, i8)>,
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

        let conn = match conn.with_attribute_server(server) {
            Ok(c) => c,
            Err(e) => {
                error!("[BLE] GATT attach failed: {:?}", e);
                continue;
            }
        };

        info!("[BLE] Connected!");
        let _ = connection_state.try_send(true);

        gatt_events_loop(
            server,
            &conn,
            tx_to_ble,
            rx_from_ble,
            battery_level,
            disconnect_cmd,
            radio_stats,
            &mut from_num,
        )
        .await;

        let _ = connection_state.try_send(false);
        info!("[BLE] Disconnected");
    }
}

async fn gatt_events_loop(
    server: &Server<'_>,
    conn: &GattConnection<'_, '_, DefaultPacketPool>,
    tx_to_ble: Receiver<'static, CriticalSectionRawMutex, FromRadioMessage, 10>,
    rx_from_ble: Sender<'static, CriticalSectionRawMutex, ToRadioMessage, 5>,
    battery_level: Receiver<'static, CriticalSectionRawMutex, u8, 1>,
    disconnect_cmd: Receiver<'static, CriticalSectionRawMutex, (), 1>,
    radio_stats: &'static Signal<CriticalSectionRawMutex, (i16, i8)>,
    from_num: &mut u32,
) {
    let mut notifications_enabled = false;

    loop {
        let tx_fut = async {
            if notifications_enabled {
                tx_to_ble.receive().await
            } else {
                core::future::pending::<FromRadioMessage>().await
            }
        };

        match select(
            radio_stats.wait(),
            select4(
                conn.next(),
                tx_fut,
                battery_level.receive(),
                disconnect_cmd.receive(),
            ),
        )
        .await
        {
            Either::First(_) => {
                // Radio stats update - could notify from_num
            }
            Either::Second(Either4::First(event)) => match event {
                GattConnectionEvent::Disconnected { reason } => {
                    info!("[BLE] Disconnected: {:?}", reason);
                    break;
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

                        // For FromRadio reads, the phone polls until empty
                        // The actual data is set via set() before notifying from_num

                        if let Err(e) = read_event.accept().map(|r| r.send()) {
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
            Either::Second(Either4::Second(msg)) => {
                // FromRadio message to send to phone
                debug!("[BLE] FromRadio: {} bytes", msg.data.len());

                // Write data to FromRadio characteristic
                // Then bump and notify FromNum to tell phone to read
                *from_num = from_num.wrapping_add(1);
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
            Either::Second(Either4::Third(_level)) => {
                // Battery level update
            }
            Either::Second(Either4::Fourth(_)) => {
                warn!("[BLE] Disconnect command from watchdog");
                break;
            }
        }
    }
}
