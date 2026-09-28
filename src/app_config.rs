//! Application-service persistent configuration.
//!
//! This is distinct from agent identity and from lifecycle/OTA state. The
//! current schema only owns the business service TCP port.

use iobewi_config_space::{Budget, ConfigSpace};
use log::warn;

use iobewi_esp_config_space::NvsConfigBackend;

const MAGIC: &[u8; 4] = b"APC1";
pub const DEFAULT_PORT: u16 = 8080;

pub const CONFIG_BUDGET: Budget = Budget::new(8);
pub type AppConfigSpace = ConfigSpace<NvsConfigBackend>;

pub async fn port(space: &AppConfigSpace) -> u16 {
    let Ok(Some(snapshot)) = space.load().await else {
        return DEFAULT_PORT;
    };
    let raw = snapshot.data;
    if raw.len() != 6 || &raw[..4] != MAGIC {
        warn!("app: stored config generation={} has an unsupported/corrupt schema", snapshot.generation);
        return DEFAULT_PORT;
    }
    u16::from_le_bytes([raw[4], raw[5]])
}

pub async fn save_port(space: &AppConfigSpace, port: u16) -> Result<(), ()> {
    let mut encoded = [0u8; 6];
    encoded[..4].copy_from_slice(MAGIC);
    encoded[4..].copy_from_slice(&port.to_le_bytes());
    space.commit(&encoded).await.map_err(|_| ())?;
    Ok(())
}
