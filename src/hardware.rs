//! Hardware-owned persistent configuration.
//!
//! The component owns the schema inside its ConfigSpace.

use iobewi_config_space::{Budget, ConfigBackend, ConfigSpace};
use log::warn;

const MAGIC: &[u8; 4] = b"HWC1";
const NONE: u8 = 0xff;

pub const CONFIG_BUDGET: Budget = Budget::new(8);
pub type HardwareConfigSpace<B> = ConfigSpace<B>;

/// Whether the hardware object exists and has the current schema. A saved
/// "LED disabled" value is still a fully provisioned hardware configuration.
pub async fn is_configured<B: ConfigBackend>(space: &HardwareConfigSpace<B>) -> bool {
    let Ok(Some(snapshot)) = space.load().await else {
        return false;
    };
    let raw = snapshot.data;
    raw.len() == 5 && &raw[..4] == MAGIC
}

pub async fn led_gpio<B: ConfigBackend>(space: &HardwareConfigSpace<B>) -> Option<u8> {
    let Ok(Some(snapshot)) = space.load().await else {
        return None;
    };
    let raw = snapshot.data;
    if raw.len() != 5 || &raw[..4] != MAGIC {
        warn!("hardware: stored config generation={} has an unsupported/corrupt schema", snapshot.generation);
        return None;
    }
    (raw[4] != NONE).then_some(raw[4])
}

pub async fn save_led_gpio<B: ConfigBackend>(
    space: &HardwareConfigSpace<B>,
    gpio: Option<u8>,
) -> Result<(), ()> {
    let mut encoded = [0u8; 5];
    encoded[..4].copy_from_slice(MAGIC);
    encoded[4] = gpio.unwrap_or(NONE);
    space.commit(&encoded).await.map_err(|_| ())?;
    Ok(())
}
