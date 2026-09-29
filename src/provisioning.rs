//! Improv Serial service: answers ESP Web Tools over any async serial
//! transport so that Wi-Fi credentials can be entered from the browser
//! instead of being baked into the firmware.
//!
//! Generic over `embedded_io_async::{Read, Write}` -- this module never
//! knows whether the real transport is ESP32 USB-Serial-JTAG, a Teensy
//! UART, USB CDC, or anything else. The platform composition root picks
//! the concrete transport and constructs [`DeviceInfo`]; this module only
//! ever encodes values it's handed.

use embedded_io_async::{Read, Write};
use log::{info, warn};

use improv_serial::{self as improv, Command, ImprovError, ParsedCommand, Parser, State};
use crate::Status;
use iobewi_indicator::StatusIndicator;
use iobewi_wifi::WifiProvisioning;

/// Device identity for Improv's `GetDeviceInfo` RPC. Owned by the platform
/// composition root -- how the chip name or a per-board device name suffix
/// is derived (efuse MAC, a serial number, a fixed string, ...) is entirely
/// its concern, never this module's.
pub struct DeviceInfo<'a> {
    pub firmware_name: &'a str,
    pub firmware_version: &'a str,
    pub chip_name: &'a str,
    pub device_name: &'a str,
}

/// Application port: "an IP-capable network is now available; start
/// whatever services depend on it." Not a hardware abstraction competing
/// with IOBEWI -- this is application orchestration (provisioning workflow
/// -> service supervisor), so it lives here rather than in `iobewi-wifi`.
/// `N` is whatever opaque network handle the Wi-Fi capability in use
/// produces (`iobewi_wifi::WifiProvisioning::NetworkHandle`); this module
/// never inspects it, only forwards it.
pub trait NetworkReady<N> {
    fn on_network_ready(&mut self, network: N);
}

/// Serves Improv Serial forever. Generic over the serial transport, the
/// Wi-Fi capability, the platform's status indicator, and the application's
/// network-ready port: this module has no idea what the real Wi-Fi backend
/// is -- see `iobewi_wifi::WifiProvisioning`.
pub async fn run<R, T, WifiT, I, S>(
    mut rx: R,
    mut tx: T,
    mut wifi: WifiT,
    mut services: S,
    indicator: &I,
    device_info: &DeviceInfo<'_>,
) -> !
where
    R: Read,
    T: Write,
    WifiT: WifiProvisioning,
    I: StatusIndicator,
    S: NetworkReady<WifiT::NetworkHandle>,
{
    let mut parser = Parser::new();
    let mut state = if wifi.is_online() {
        State::Provisioned
    } else {
        State::Authorized
    };
    let mut buffer = [0u8; 64];

    info!("Improv: listening for commands");
    indicator.set(idle_status(&wifi));

    loop {
        let read = match rx.read(&mut buffer).await {
            Ok(read) => read,
            Err(e) => {
                warn!("serial read failed: {e:?}");
                continue;
            }
        };
        for &byte in &buffer[..read] {
            if let Some(command) = parser.feed(byte) {
                handle(command, &mut tx, &mut state, &mut wifi, &mut services, indicator, device_info).await;
            }
        }
    }
}

/// What the LED shows once a transient action (a scan) is over.
fn idle_status<WifiT: WifiProvisioning>(wifi: &WifiT) -> Status {
    if wifi.is_online() {
        Status::Online
    } else {
        Status::Ready
    }
}

async fn send<T: Write>(tx: &mut T, frame: &[u8]) {
    if let Err(e) = tx.write_all(frame).await {
        warn!("serial write failed: {e:?}");
    }
}

/// The device's own HTTPS provisioning page (`src/http/`), reachable once Wi-Fi
/// is up. ESP Web Tools' client reads this from the first string in a
/// WifiSettings or (if already provisioned) GetCurrentState RPC response
/// and shows it as a "Visit Device" link.
fn next_url<WifiT: WifiProvisioning>(wifi: &WifiT) -> alloc::string::String {
    wifi.address()
        .map(|address| alloc::format!("https://{address}/"))
        .unwrap_or_default()
}

async fn handle<T: Write, WifiT: WifiProvisioning, I: StatusIndicator, S: NetworkReady<WifiT::NetworkHandle>>(
    command: ParsedCommand,
    tx: &mut T,
    state: &mut State,
    wifi: &mut WifiT,
    services: &mut S,
    indicator: &I,
    device_info: &DeviceInfo<'_>,
) {
    match command {
        ParsedCommand::GetCurrentState => {
            send(tx, &improv::state_frame(*state)).await;
            // The browser client's `requestCurrentState()` does something
            // easy to miss: when the device is *already* Provisioned at
            // connect time, it doesn't just wait for this CurrentState
            // broadcast -- it also awaits an RPC_RESULT reply to this same
            // GET_CURRENT_STATE request, to pick up `nextUrl` from it. If
            // that reply never comes, that `await` just hangs until the
            // client's own internal ~30s RPC timeout, well past the ~1.5s
            // the *outer* connection check allows, so the browser reports
            // "Improv Wi-Fi Serial not detected" despite everything above
            // having worked. This path was never exercised before
            // `WifiManager::reconnect_saved` existed, since state always
            // started at Authorized then -- verified against ESP Web
            // Tools' actual (unminified-by-us) client source, not guessed.
            if *state == State::Provisioned {
                send(tx, &improv::rpc_response_frame(Command::GetCurrentState, &[next_url(wifi).as_bytes()])).await;
            }
        }
        ParsedCommand::GetDeviceInfo => {
            let frame = improv::rpc_response_frame(
                Command::GetDeviceInfo,
                &[
                    device_info.firmware_name.as_bytes(),
                    device_info.firmware_version.as_bytes(),
                    device_info.chip_name.as_bytes(),
                    device_info.device_name.as_bytes(),
                ],
            );
            send(tx, &frame).await;
        }
        ParsedCommand::GetWifiNetworks => {
            indicator.set(Status::Scanning);
            for network in wifi.scan().await {
                let signal_strength = alloc::format!("{}", network.signal_strength);
                let secured: &[u8] = if network.secured { b"YES" } else { b"NO" };
                let frame = improv::rpc_response_frame(
                    Command::GetWifiNetworks,
                    &[network.ssid.as_bytes(), signal_strength.as_bytes(), secured],
                );
                send(tx, &frame).await;
            }
            // An empty entry terminates the list.
            send(tx, &improv::rpc_response_frame(Command::GetWifiNetworks, &[])).await;
            indicator.set(idle_status(wifi));
        }
        ParsedCommand::GetNetworkState => {
            let mut flags: u8 = 0x02; // supports Wi-Fi
            let online = wifi.is_online();
            if online {
                flags |= 0x01; // online
            }
            let flags = alloc::format!("{flags}");
            // ESPHome's reference component (improv_serial_component.cpp)
            // appends the device URL here too, when online -- matches
            // GetCurrentState/WifiSettings above.
            let frame = if online {
                improv::rpc_response_frame(
                    Command::GetNetworkState,
                    &[flags.as_bytes(), next_url(wifi).as_bytes()],
                )
            } else {
                improv::rpc_response_frame(Command::GetNetworkState, &[flags.as_bytes()])
            };
            send(tx, &frame).await;
        }
        ParsedCommand::WifiSettings(settings) => {
            info!("Improv: connecting to SSID={}", settings.ssid);
            *state = State::Provisioning;
            indicator.set(Status::Connecting);
            send(tx, &improv::state_frame(*state)).await;

            if wifi.provision(&settings.ssid, settings.password).await {
                if let Some(network) = wifi.network_handle() {
                    services.on_network_ready(network);
                } else {
                    warn!("Wi-Fi reported connected without a network handle");
                }
                *state = State::Provisioned;
                indicator.set(Status::Online);
                send(tx, &improv::state_frame(*state)).await;
                send(
                    tx,
                    &improv::rpc_response_frame(Command::WifiSettings, &[next_url(wifi).as_bytes()]),
                )
                .await;
            } else {
                *state = State::Authorized;
                indicator.set(Status::Failed);
                send(tx, &improv::error_frame(ImprovError::UnableToConnect)).await;
                send(tx, &improv::state_frame(*state)).await;
            }
        }
        ParsedCommand::Unsupported(command) => {
            info!("Improv: unsupported command 0x{command:02X}");
            send(tx, &improv::error_frame(ImprovError::UnknownRpc)).await;
        }
    }
}
