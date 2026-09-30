//! Runtime McuConfigMap storage behind `GET/POST /v1alpha1/config`.
//!
//! The contract intentionally keeps this map schema-free: the Core validates
//! semantics, while the agent persists opaque UTF-8 key/value pairs and the
//! application consumes the active snapshot loaded at boot. Persistence is a
//! single atomic ConfigSpace blob rather than one NVS key per entry.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use iobewi_config_space::{Budget, ConfigBackend, ConfigSpace};
use log::warn;
use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 4] = b"RCF1";
const HEADER_LEN: usize = 6;
const MAX_KEY_LEN: usize = 15;
const MAX_VALUE_LEN: usize = 63;

/// Maximum serialized McuConfigMap payload. This bounds fleet-supplied runtime
/// configuration while keeping NVS admission deterministic at boot.
pub const CONFIG_BUDGET: Budget = Budget::new(2048);
pub type RuntimeConfigSpace<B> = ConfigSpace<B>;

#[derive(Debug)]
pub enum RuntimeConfigError {
    Persistence,
    Corrupt,
    TooLarge,
}

#[derive(Deserialize)]
pub struct ConfigPush {
    pub data: BTreeMap<String, String>,
}

#[derive(Serialize)]
pub struct ConfigView {
    generation: u64,
    active_generation: u64,
    active: BTreeMap<String, String>,
    nvs: BTreeMap<String, String>,
}

pub struct RuntimeConfig<B: ConfigBackend> {
    space: RuntimeConfigSpace<B>,
    active_generation: u64,
    active: BTreeMap<String, String>,
}

fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= MAX_KEY_LEN && !key.starts_with('_')
}

fn encode(map: &BTreeMap<String, String>) -> Option<Vec<u8>> {
    let count = u16::try_from(map.len()).ok()?;
    let mut total = HEADER_LEN;
    for (key, value) in map {
        if !valid_key(key) || value.len() > MAX_VALUE_LEN {
            return None;
        }
        total = total
            .checked_add(2)?
            .checked_add(key.len())?
            .checked_add(value.len())?;
    }
    if total > CONFIG_BUDGET.max_bytes() {
        return None;
    }

    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&count.to_le_bytes());
    for (key, value) in map {
        out.push(key.len() as u8);
        out.push(value.len() as u8);
        out.extend_from_slice(key.as_bytes());
        out.extend_from_slice(value.as_bytes());
    }
    Some(out)
}

fn decode(raw: &[u8]) -> Option<BTreeMap<String, String>> {
    if raw.len() < HEADER_LEN || &raw[..4] != MAGIC {
        return None;
    }
    let count = u16::from_le_bytes([raw[4], raw[5]]) as usize;
    let mut offset = HEADER_LEN;
    let mut out = BTreeMap::new();

    for _ in 0..count {
        if offset.checked_add(2)? > raw.len() {
            return None;
        }
        let key_len = raw[offset] as usize;
        let value_len = raw[offset + 1] as usize;
        offset += 2;
        if key_len == 0 || key_len > MAX_KEY_LEN || value_len > MAX_VALUE_LEN {
            return None;
        }
        let key_end = offset.checked_add(key_len)?;
        let value_end = key_end.checked_add(value_len)?;
        if value_end > raw.len() {
            return None;
        }
        let key = core::str::from_utf8(&raw[offset..key_end]).ok()?;
        let value = core::str::from_utf8(&raw[key_end..value_end]).ok()?;
        if !valid_key(key) || out.insert(String::from(key), String::from(value)).is_some() {
            return None;
        }
        offset = value_end;
    }

    (offset == raw.len()).then_some(out)
}

impl<B: ConfigBackend> RuntimeConfig<B> {
    /// Takes the contract's "active" snapshot once at boot. Later POSTs only
    /// replace the persisted value; this snapshot changes on the next reboot.
    pub async fn new(space: RuntimeConfigSpace<B>) -> Result<Self, RuntimeConfigError> {
        match space.load().await {
            Ok(Some(snapshot)) => {
                let active = decode(&snapshot.data).ok_or(RuntimeConfigError::Corrupt)?;
                Ok(Self {
                    space,
                    active_generation: snapshot.generation,
                    active,
                })
            }
            Ok(None) => Ok(Self {
                space,
                active_generation: 0,
                active: BTreeMap::new(),
            }),
            Err(_) => Err(RuntimeConfigError::Persistence),
        }
    }

    pub fn active_generation(&self) -> u64 {
        self.active_generation
    }

    pub fn active(&self) -> &BTreeMap<String, String> {
        &self.active
    }

    async fn current(&self) -> Result<(u64, BTreeMap<String, String>), RuntimeConfigError> {
        match self.space.load().await {
            Ok(Some(snapshot)) => {
                let map = decode(&snapshot.data).ok_or(RuntimeConfigError::Corrupt)?;
                Ok((snapshot.generation, map))
            }
            Ok(None) => Ok((0, BTreeMap::new())),
            Err(_) => Err(RuntimeConfigError::Persistence),
        }
    }

    pub async fn generation(&self) -> u64 {
        match self.current().await {
            Ok((generation, _)) => generation,
            Err(e) => {
                warn!("runtime config: generation read failed: {e:?}");
                self.active_generation
            }
        }
    }

    pub async fn view(&self) -> Result<ConfigView, RuntimeConfigError> {
        let (generation, nvs) = self.current().await?;
        Ok(ConfigView {
            generation,
            active_generation: self.active_generation,
            active: self.active.clone(),
            nvs,
        })
    }

    /// Merge-on-key contract semantics. Empty values remove a key. Invalid
    /// keys/values are ignored, matching the historical agent behaviour.
    /// The whole resulting map is committed atomically as one ConfigSpace.
    pub async fn apply(&self, push: &ConfigPush) -> Result<u64, RuntimeConfigError> {
        let (generation, mut current) = self.current().await?;
        let mut changed = false;

        for (key, value) in &push.data {
            if !valid_key(key) || value.len() > MAX_VALUE_LEN {
                continue;
            }
            if value.is_empty() {
                changed |= current.remove(key).is_some();
            } else if current.get(key) != Some(value) {
                current.insert(key.clone(), value.clone());
                changed = true;
            }
        }

        if !changed {
            return Ok(generation);
        }

        let encoded = encode(&current).ok_or(RuntimeConfigError::TooLarge)?;
        self.space
            .commit(&encoded)
            .await
            .map_err(|_| RuntimeConfigError::Persistence)
    }
}
