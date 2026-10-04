//! Wi-Fi for the POC: Improv Serial provisioning (ESP Web Tools) on top of IOBEWI's portable
//! `WifiManager`, with credentials persisted by `flash_config`. Nothing is baked in at build time.
//!
//! The ESP-specific parts (radio transport, UART) are adapters local to this POC because
//! IOBEWI's ESP adapters pin a different esp-hal than the rest of the POC.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use embassy_futures::select::{Either, select};
use embassy_net::Stack;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::{Async, uart::{UartRx, UartTx}};
use esp_radio::wifi::{
    AuthenticationMethod, AuthenticationMethodConfig, Config as WifiConfig, WifiController,
    scan::ScanConfig, sta::StationConfig,
};
use improv_serial::{self as improv, Command, ImprovError, ParsedCommand, Parser, State};
use iobewi_wifi_core::{Network, WifiProvisioning, WifiTransport};
use iobewi_wifi_manager::{LinkObserver, MaintainError, Sleep, WifiManager};

use crate::flash_config::FlashConfigBackend;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DHCP_TIMEOUT: Duration = Duration::from_secs(20);

pub struct EspWifiTransport {
    controller: WifiController<'static>,
    stack: Stack<'static>,
    current: Option<(String, String)>,
}

impl EspWifiTransport {
    pub fn new(controller: WifiController<'static>, stack: Stack<'static>) -> Self {
        Self { controller, stack, current: None }
    }
}

impl WifiTransport for EspWifiTransport {
    type Address = embassy_net::Ipv4Address;
    type NetworkHandle = Stack<'static>;

    async fn connect(&mut self, ssid: &str, password: String) -> bool {
        // Already up on these exact credentials (e.g. maintain() restarted after an Improv
        // request): do not bounce the link under a running stream.
        if self.stack.is_config_up() {
            if let Some((s, p)) = &self.current {
                if s == ssid && *p == password {
                    return true;
                }
            }
        }
        self.current = None;

        let Ok(ssid_cfg) = ssid.try_into() else {
            esp_println::println!("wifi: invalid ssid");
            return false;
        };
        let station = StationConfig::default().with_ssid(ssid_cfg);
        let station = if password.is_empty() {
            station
        } else {
            let Ok(pw) = password.as_str().try_into() else {
                esp_println::println!("wifi: invalid password");
                return false;
            };
            station.with_authentication(AuthenticationMethodConfig::Wpa2Personal(pw))
        };

        // Err(NotConnected) when idle is expected.
        let _ = self.controller.disconnect_async().await;
        if self.controller.set_config(&WifiConfig::Station(station)).is_err() {
            esp_println::println!("wifi: set_config failed");
            return false;
        }

        esp_println::println!("wifi: connecting ssid={}", ssid);
        match with_timeout(CONNECT_TIMEOUT, self.controller.connect_async()).await {
            Ok(Ok(info)) => esp_println::println!("wifi: associated {:?}", info),
            Ok(Err(err)) => {
                esp_println::println!("wifi: connect error {:?}", err);
                return false;
            }
            Err(_) => {
                esp_println::println!("wifi: connect timeout");
                let _ = self.controller.disconnect_async().await;
                return false;
            }
        }
        if with_timeout(DHCP_TIMEOUT, self.stack.wait_config_up()).await.is_err() {
            esp_println::println!("wifi: DHCP timeout");
            let _ = self.controller.disconnect_async().await;
            return false;
        }
        if let Some(config) = self.stack.config_v4() {
            esp_println::println!("wifi: got IP {}", config.address);
        }
        self.current = Some((ssid.to_string(), password));
        true
    }

    async fn scan(&mut self) -> Vec<Network> {
        let config = ScanConfig::default().with_max(20);
        match self.controller.scan_async(&config).await {
            Ok(list) => list
                .into_iter()
                .map(|ap| Network {
                    ssid: ap.ssid.as_str().to_string(),
                    signal_strength: ap.signal_strength,
                    secured: !matches!(ap.auth_method, None | Some(AuthenticationMethod::None)),
                })
                .collect(),
            Err(err) => {
                esp_println::println!("wifi: scan error {:?}", err);
                Vec::new()
            }
        }
    }

    async fn wait_down(&mut self) {
        if !self.stack.is_config_up() {
            return;
        }
        // Radio link lost, or DHCP/IP configuration lost.
        let _ = select(
            self.controller.wait_for_disconnect_async(),
            self.stack.wait_config_down(),
        )
        .await;
        self.current = None;
    }

    fn ip(&self) -> Option<Self::Address> {
        self.stack.config_v4().map(|c| c.address.address())
    }

    fn network_handle(&self) -> Option<Self::NetworkHandle> {
        self.stack.is_config_up().then_some(self.stack)
    }

    fn is_online(&self) -> bool {
        self.stack.is_config_up()
    }
}

struct EmbassySleep;

impl Sleep for EmbassySleep {
    async fn sleep_ms(&self, ms: u32) {
        Timer::after(Duration::from_millis(ms as u64)).await;
    }
}

struct LogObserver;

impl LinkObserver<Stack<'static>> for LogObserver {
    fn link_down(&mut self) {
        esp_println::println!("wifi: link down, reconnecting");
    }
    fn ready(&mut self, _network: Stack<'static>) {
        esp_println::println!("wifi: ready");
    }
}

pub type Manager = WifiManager<EspWifiTransport, FlashConfigBackend>;

static IMPROV_COMMANDS: Channel<CriticalSectionRawMutex, ParsedCommand, 2> = Channel::new();

/// Reads UART0 (the USB-UART port that ESP Web Tools talks to) and forwards parsed Improv
/// commands. Log lines on the same wire are ignored by the parser (it resynchronises).
pub async fn improv_reader(mut rx: UartRx<'static, Async>) -> ! {
    let mut parser = Parser::new();
    let mut buf = [0u8; 64];
    loop {
        match rx.read_async(&mut buf).await {
            Ok(n) => {
                for &byte in &buf[..n] {
                    if let Some(command) = parser.feed(byte) {
                        IMPROV_COMMANDS.send(command).await;
                    }
                }
            }
            Err(_) => Timer::after(Duration::from_millis(10)).await,
        }
    }
}

/// Owns the Wi-Fi manager: keeps the saved network connected and serves Improv requests.
/// A request pre-empts `maintain()`; it restarts afterwards (and `connect` is a no-op when the
/// link is already up on the same credentials).
pub async fn wifi_task(mut manager: Manager, mut tx: UartTx<'static, Async>) -> ! {
    let sleep = EmbassySleep;
    let mut observer = LogObserver;
    let mut state = State::Authorized;

    loop {
        *(&mut state) = if manager.is_online() { State::Provisioned } else { State::Authorized };
        match select(
            manager.maintain(&sleep, &mut observer),
            IMPROV_COMMANDS.receive(),
        )
        .await
        {
            Either::First(MaintainError::NotProvisioned) => {
                esp_println::println!("wifi: no saved credentials; waiting for Improv provisioning");
                let command = IMPROV_COMMANDS.receive().await;
                handle(command, &mut tx, &mut state, &mut manager).await;
            }
            Either::Second(command) => handle(command, &mut tx, &mut state, &mut manager).await,
        }
    }
}

async fn send(tx: &mut UartTx<'static, Async>, frame: &[u8]) {
    let mut rest = frame;
    while !rest.is_empty() {
        match tx.write_async(rest).await {
            Ok(n) => rest = &rest[n..],
            Err(_) => return,
        }
    }
    let _ = tx.flush_async().await;
}

async fn handle(
    command: ParsedCommand,
    tx: &mut UartTx<'static, Async>,
    state: &mut State,
    manager: &mut Manager,
) {
    match command {
        ParsedCommand::GetCurrentState => {
            send(tx, &improv::state_frame(*state)).await;
            // ESP Web Tools also awaits an RPC result when already provisioned.
            if *state == State::Provisioned {
                send(tx, &improv::rpc_response_frame(Command::GetCurrentState, &[])).await;
            }
        }
        ParsedCommand::GetDeviceInfo => {
            let frame = improv::rpc_response_frame(
                Command::GetDeviceInfo,
                &[b"usb-radio-poc", b"0.2.0", b"ESP32-S3", b"usb-radio"],
            );
            send(tx, &frame).await;
        }
        ParsedCommand::GetWifiNetworks => {
            for network in WifiProvisioning::scan(manager).await {
                let signal = network.signal_strength.to_string();
                let secured: &[u8] = if network.secured { b"YES" } else { b"NO" };
                let frame = improv::rpc_response_frame(
                    Command::GetWifiNetworks,
                    &[network.ssid.as_bytes(), signal.as_bytes(), secured],
                );
                send(tx, &frame).await;
            }
            send(tx, &improv::rpc_response_frame(Command::GetWifiNetworks, &[])).await;
        }
        ParsedCommand::GetNetworkState => {
            let flags: &[u8] = if manager.is_online() { b"3" } else { b"2" };
            send(tx, &improv::rpc_response_frame(Command::GetNetworkState, &[flags])).await;
        }
        ParsedCommand::WifiSettings(settings) => {
            esp_println::println!("improv: provisioning ssid={}", settings.ssid);
            *state = State::Provisioning;
            send(tx, &improv::state_frame(*state)).await;
            if WifiProvisioning::provision(manager, &settings.ssid, settings.password).await {
                esp_println::println!("improv: provisioned, credentials saved");
                *state = State::Provisioned;
                send(tx, &improv::state_frame(*state)).await;
                send(tx, &improv::rpc_response_frame(Command::WifiSettings, &[])).await;
            } else {
                esp_println::println!("improv: provisioning failed");
                *state = State::Authorized;
                send(tx, &improv::error_frame(ImprovError::UnableToConnect)).await;
                send(tx, &improv::state_frame(*state)).await;
            }
        }
        ParsedCommand::Unsupported(command) => {
            esp_println::println!("improv: unsupported command 0x{:02X}", command);
            send(tx, &improv::error_frame(ImprovError::UnknownRpc)).await;
        }
    }
}
