//! Wi-Fi for the POC: Improv Serial provisioning (ESP Web Tools) on top of IOBEWI's portable
//! `WifiManager`, with credentials persisted by `flash_config`. Nothing is baked in at build time.
//!
//! Radio mechanics use IOBEWI's ESP adapter. The product wrapper preserves the
//! golden no-op for unchanged credentials on an online link; UART remains local.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use embassy_futures::select::{Either, select};
use embassy_net::Stack;
use embassy_sync::{
    blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex},
    channel::Channel,
    signal::Signal,
};
use embassy_time::{Duration, Timer};
use esp_hal::{
    Async,
    uart::{UartRx, UartTx},
};
use improv_serial::{self as improv, Command, ImprovError, ParsedCommand, Parser, State};
use iobewi_wifi_core::{Network, WifiProvisioning, WifiTransport};
use iobewi_wifi_manager::{LinkObserver, MaintainError, Sleep, WifiManager};

use crate::flash_config::FlashConfigBackend;

pub struct EspWifiTransport {
    inner: iobewi_esp_wifi::WifiManager<3>,
    current: Option<(String, String)>,
}

impl EspWifiTransport {
    pub fn new(inner: iobewi_esp_wifi::WifiManager<3>) -> Self {
        Self {
            inner,
            current: None,
        }
    }
}

impl WifiTransport for EspWifiTransport {
    type Address = embassy_net::Ipv4Address;
    type NetworkHandle = Stack<'static>;

    async fn connect(&mut self, ssid: &str, password: String) -> bool {
        // Restarting maintain after an Improv request must not bounce live audio.
        if self.inner.is_online()
            && self
                .current
                .as_ref()
                .is_some_and(|(s, p)| s == ssid && *p == password)
        {
            return true;
        }
        self.current = None;
        if !self.inner.connect(ssid, password.clone()).await {
            return false;
        }
        self.current = Some((ssid.to_string(), password));
        true
    }

    async fn scan(&mut self) -> Vec<Network> {
        self.inner.scan().await
    }

    async fn wait_down(&mut self) {
        WifiTransport::wait_down(&mut self.inner).await;
        self.current = None;
    }

    fn ip(&self) -> Option<Self::Address> {
        self.inner.ip()
    }
    fn network_handle(&self) -> Option<Self::NetworkHandle> {
        self.inner.network_handle()
    }
    fn is_online(&self) -> bool {
        self.inner.is_online()
    }
}

pub type NetworkReady = Signal<NoopRawMutex, Stack<'static>>;

struct EmbassySleep;

impl Sleep for EmbassySleep {
    async fn sleep_ms(&self, ms: u32) {
        Timer::after(Duration::from_millis(ms as u64)).await;
    }
}

struct LogObserver(&'static NetworkReady);

impl LinkObserver<Stack<'static>> for LogObserver {
    fn link_down(&mut self) {
        esp_println::println!("wifi: link down, reconnecting");
    }
    fn ready(&mut self, network: Stack<'static>) {
        self.0.signal(network);
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
pub async fn wifi_task(
    mut manager: Manager,
    mut tx: UartTx<'static, Async>,
    network_ready: &'static NetworkReady,
) -> ! {
    let sleep = EmbassySleep;
    let mut observer = LogObserver(network_ready);
    let mut state = State::Authorized;

    loop {
        *(&mut state) = if manager.is_online() {
            State::Provisioned
        } else {
            State::Authorized
        };
        match select(
            manager.maintain(&sleep, &mut observer),
            IMPROV_COMMANDS.receive(),
        )
        .await
        {
            Either::First(MaintainError::NotProvisioned) => {
                esp_println::println!(
                    "wifi: no saved credentials; waiting for Improv provisioning"
                );
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
                send(
                    tx,
                    &improv::rpc_response_frame(Command::GetCurrentState, &[]),
                )
                .await;
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
            send(
                tx,
                &improv::rpc_response_frame(Command::GetWifiNetworks, &[]),
            )
            .await;
        }
        ParsedCommand::GetNetworkState => {
            let flags: &[u8] = if manager.is_online() { b"3" } else { b"2" };
            send(
                tx,
                &improv::rpc_response_frame(Command::GetNetworkState, &[flags]),
            )
            .await;
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
