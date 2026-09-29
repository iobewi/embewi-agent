//! Improv Serial service: answers ESP Web Tools over USB-Serial-JTAG so that
//! Wi-Fi credentials can be entered from the browser instead of being baked
//! into the firmware.

use embedded_io_async::{Read, Write};
use esp_hal::Async;
use esp_hal::usb_serial_jtag::{UsbSerialJtagRx, UsbSerialJtagTx};
use log::{info, warn};

use improv_serial::{self as improv, Command, ImprovError, ParsedCommand, Parser, State};
use crate::Status;
use crate::supervisor::ProvisioningSupervisor;
use crate::wifi::WifiManager;
use iobewi_indicator::StatusIndicator;

const NAME: &str = "embewi-init";
/// Not a literal: this tracks whichever chip `esp-hal`'s own feature flags
/// (in Cargo.toml) are actually built for, so it can't drift when the
/// target changes -- e.g. from ESP32-C3 to ESP32-S3.
const CHIP: &str = esp_metadata_generated::chip_pretty!();

type Rx = UsbSerialJtagRx<'static, Async>;
type Tx = UsbSerialJtagTx<'static, Async>;

/// Serves Improv Serial forever. Generic over the platform's status
/// indicator: this module expresses only logical states (`Status::*`), never
/// a colour or a peripheral -- see `iobewi_indicator::StatusIndicator`.
pub async fn run<I: StatusIndicator>(
    mut rx: Rx,
    mut tx: Tx,
    mut wifi: WifiManager,
    mut supervisor: ProvisioningSupervisor,
    indicator: &I,
) -> ! {
    let mut parser = Parser::new();
    let mut state = if wifi.is_online() {
        State::Provisioned
    } else {
        State::Authorized
    };
    let mut buffer = [0u8; 64];

    info!("Improv: listening on USB-Serial-JTAG");
    indicator.set(idle_status(&wifi));

    loop {
        let read = match rx.read(&mut buffer).await {
            Ok(read) => read,
            Err(e) => {
                warn!("USB read failed: {e:?}");
                continue;
            }
        };
        for &byte in &buffer[..read] {
            if let Some(command) = parser.feed(byte) {
                handle(command, &mut tx, &mut state, &mut wifi, &mut supervisor, indicator).await;
            }
        }
    }
}

/// What the LED shows once a transient action (a scan) is over.
fn idle_status(wifi: &WifiManager) -> Status {
    if wifi.is_online() {
        Status::Online
    } else {
        Status::Ready
    }
}

async fn send(tx: &mut Tx, frame: &[u8]) {
    if let Err(e) = tx.write_all(frame).await {
        warn!("USB write failed: {e:?}");
    }
}

/// The device's own HTTPS provisioning page (`src/http/`), reachable once Wi-Fi
/// is up. ESP Web Tools' client reads this from the first string in a
/// WifiSettings or (if already provisioned) GetCurrentState RPC response
/// and shows it as a "Visit Device" link.
fn next_url(wifi: &WifiManager) -> alloc::string::String {
    wifi.ip()
        .map(|ip| alloc::format!("https://{ip}/"))
        .unwrap_or_default()
}

async fn handle<I: StatusIndicator>(
    command: ParsedCommand,
    tx: &mut Tx,
    state: &mut State,
    wifi: &mut WifiManager,
    supervisor: &mut ProvisioningSupervisor,
    indicator: &I,
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
            // Device name = NAME + a suffix from the efuse-burned MAC address,
            // unique per physical board (unlike NAME, which is the same for
            // every unit running this firmware). Matches the convention seen
            // in ESPHome's own Improv device info (e.g. "...-d5eb28").
            let mac = esp_hal::efuse::base_mac_address();
            let mac = mac.as_bytes();
            let device_name =
                alloc::format!("{NAME}-{:02x}{:02x}{:02x}", mac[3], mac[4], mac[5]);
            let frame = improv::rpc_response_frame(
                Command::GetDeviceInfo,
                &[
                    NAME.as_bytes(),
                    env!("CARGO_PKG_VERSION").as_bytes(),
                    CHIP.as_bytes(),
                    device_name.as_bytes(),
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
                if let Some(stack) = wifi.ip_stack() {
                    supervisor.on_ip_ready(stack);
                } else {
                    warn!("Wi-Fi reported connected without an IP stack");
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
