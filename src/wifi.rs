//! Wi-Fi connector integration.
//!
//! Radio/network mechanics live in espbewi-wifi. Persistent configuration
//! is owned by this component through one isolated config-space-manager
//! capability; embewi-agent no longer reads or writes Wi-Fi credentials on
//! the normal path.

use alloc::string::String;
use alloc::vec::Vec;

use config_space_manager::{Budget, ConfigSpace};
use embassy_executor::Spawner;
use embassy_net::{Stack, StackResources};
use esp_hal::peripherals::WIFI;
use log::{info, warn};
use static_cell::StaticCell;

use config_space_manager_esp_nvs::NvsConfigBackend;

pub use espbewi_wifi::Network;

const CONFIG_MAGIC: &[u8; 4] = b"WFC1";
const CONFIG_HEADER_LEN: usize = 6;

/// Maximum serialized Wi-Fi component configuration.
///
/// Current station credentials need at most 4-byte magic + two length bytes
/// + 32-byte SSID + 64-byte password. 128 bytes leaves room for a small
/// schema evolution without changing the boot-time reservation.
pub const CONFIG_BUDGET: Budget = Budget::new(128);

// DHCP (1) + admin HTTP/HTTPS (1) + SNTP UDP (1) + transient outbound DNS
// (1) + heartbeat TCP (1) + log-stream WS/TCP (1), with headroom for OTA.
// This sizing remains application integration policy; it is unrelated to
// credential persistence.
const SOCKETS: usize = 8;
static RESOURCES: StaticCell<StackResources<SOCKETS>> = StaticCell::new();

#[derive(Clone)]
struct WifiConfig {
    ssid: String,
    password: String,
}

impl WifiConfig {
    fn encode(&self) -> Option<Vec<u8>> {
        let ssid_len = u8::try_from(self.ssid.len()).ok()?;
        let password_len = u8::try_from(self.password.len()).ok()?;
        let total = CONFIG_HEADER_LEN
            .checked_add(self.ssid.len())?
            .checked_add(self.password.len())?;
        if total > CONFIG_BUDGET.max_bytes() {
            return None;
        }

        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(CONFIG_MAGIC);
        out.push(ssid_len);
        out.push(password_len);
        out.extend_from_slice(self.ssid.as_bytes());
        out.extend_from_slice(self.password.as_bytes());
        Some(out)
    }

    fn decode(raw: &[u8]) -> Option<Self> {
        if raw.len() < CONFIG_HEADER_LEN || &raw[..4] != CONFIG_MAGIC {
            return None;
        }
        let ssid_len = raw[4] as usize;
        let password_len = raw[5] as usize;
        let expected = CONFIG_HEADER_LEN
            .checked_add(ssid_len)?
            .checked_add(password_len)?;
        if raw.len() != expected {
            return None;
        }

        let ssid_end = CONFIG_HEADER_LEN + ssid_len;
        let ssid = core::str::from_utf8(&raw[CONFIG_HEADER_LEN..ssid_end]).ok()?;
        let password = core::str::from_utf8(&raw[ssid_end..]).ok()?;
        Some(Self {
            ssid: String::from(ssid),
            password: String::from(password),
        })
    }
}

pub type WifiConfigSpace = ConfigSpace<NvsConfigBackend>;

/// Whether durable station credentials exist and decode correctly.
pub async fn is_provisioned(space: &WifiConfigSpace) -> bool {
    match space.load().await {
        Ok(Some(snapshot)) => WifiConfig::decode(&snapshot.data)
            .is_some_and(|config| !config.ssid.is_empty()),
        _ => false,
    }
}

pub struct WifiManager {
    transport: espbewi_wifi::WifiManager<SOCKETS>,
    config: WifiConfigSpace,
}

impl WifiManager {
    pub fn new(
        peripheral: WIFI<'static>,
        spawner: Spawner,
        config: WifiConfigSpace,
    ) -> Self {
        Self {
            transport: espbewi_wifi::WifiManager::new(
                peripheral,
                spawner,
                RESOURCES.init(StackResources::new()),
            ),
            config,
        }
    }

    async fn saved_config(&self) -> Option<WifiConfig> {
        match self.config.load().await {
            Ok(Some(snapshot)) => match WifiConfig::decode(&snapshot.data) {
                Some(config) => Some(config),
                None => {
                    warn!(
                        "Wi-Fi: stored config generation={} has an unsupported/corrupt schema",
                        snapshot.generation
                    );
                    None
                }
            },
            Ok(None) => None,
            Err(e) => {
                warn!("Wi-Fi: config-space load failed: {e:?}");
                None
            }
        }
    }

    /// Reconnects using this component's own persisted configuration.
    pub async fn reconnect_saved(&mut self) -> bool {
        let Some(config) = self.saved_config().await else {
            return false;
        };

        info!("Wi-Fi: reconnecting to saved SSID={}", config.ssid);
        self.transport.connect(&config.ssid, config.password).await
    }

    async fn restore_previous(&mut self, previous: Option<WifiConfig>) {
        let Some(previous) = previous else {
            return;
        };
        info!("Wi-Fi: restoring previous SSID={} after failed reprovision", previous.ssid);
        if !self
            .transport
            .connect(&previous.ssid, previous.password)
            .await
        {
            warn!("Wi-Fi: previous network could not be restored");
        }
    }

    /// Tests candidate credentials first and publishes them only after
    /// association + DHCP succeeded.
    ///
    /// If the candidate connection or durable config commit fails, the old
    /// persisted configuration remains authoritative and is immediately
    /// reconnected instead of leaving the running device offline until reboot.
    pub async fn provision(&mut self, ssid: &str, password: String) -> bool {
        let previous = self.saved_config().await;
        if !self.transport.connect(ssid, password.clone()).await {
            self.restore_previous(previous).await;
            return false;
        }

        let candidate = WifiConfig {
            ssid: String::from(ssid),
            password,
        };
        let Some(encoded) = candidate.encode() else {
            warn!("Wi-Fi: candidate credentials exceed config-space schema limits");
            self.restore_previous(previous).await;
            return false;
        };

        match self.config.commit(&encoded).await {
            Ok(generation) => {
                info!("Wi-Fi: configuration committed generation={generation}");
                true
            }
            Err(e) => {
                warn!("Wi-Fi: connected, but durable config commit failed: {e:?}");
                self.restore_previous(previous).await;
                false
            }
        }
    }

    pub async fn scan(&mut self) -> Vec<Network> {
        self.transport.scan().await
    }

    pub fn ip(&self) -> Option<embassy_net::Ipv4Address> {
        self.transport.ip()
    }

    pub fn ip_stack(&self) -> Option<Stack<'static>> {
        self.transport.ip_stack()
    }

    pub fn is_online(&self) -> bool {
        self.transport.is_online()
    }
}
