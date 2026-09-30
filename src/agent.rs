//! Embewi contract v1alpha1 -- inbound API surface (Core -> ESP), §4.
//!
//! `Info::staged`/`active_slot`/`firmware.digest`/`state` are backed by
//! `src/ota.rs` (contrat §3/§6) -- this module just assembles the JSON
//! shapes, `ota.rs` owns the actual OTA state machine.

use alloc::string::String;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicU8, Ordering};

use iobewi_config_space::{Budget, ConfigBackend, ConfigSpace};
use serde::Serialize;
use subtle::ConstantTimeEq;

/// Versions of the `/v1alpha1`-style protocol this agent answers, highest
/// first (contrat §4, "Découverte de version d'API").
pub const API_VERSIONS: &[&str] = &["v1alpha1"];
pub const FW_NAME: &str = "embewi-agent-esp";
pub const FW_VERSION: &str = env!("CARGO_PKG_VERSION");

const CONFIG_MAGIC: &[u8; 4] = b"AGC1";
const CONFIG_HEADER_LEN: usize = 10;
const MAX_NODE_ID_LEN: usize = 64;
const MAX_CTRL_URL_LEN: usize = 192;
const MAX_TOKEN_LEN: usize = 64;

/// Reserved opaque storage for the agent identity/configuration domain.
/// The component owns the schema; IOBEWI ConfigSpace only owns isolation,
/// capacity admission and complete-value replacement.
pub const CONFIG_BUDGET: Budget = Budget::new(384);
pub type AgentConfigSpace<B> = ConfigSpace<B>;

#[derive(Clone, Default)]
struct AgentConfig {
    node_id: String,
    ctrl_url: String,
    token: String,
}

impl AgentConfig {
    fn encode(&self) -> Option<alloc::vec::Vec<u8>> {
        if self.node_id.len() > MAX_NODE_ID_LEN
            || self.ctrl_url.len() > MAX_CTRL_URL_LEN
            || self.token.len() > MAX_TOKEN_LEN
        {
            return None;
        }
        let node_len = u16::try_from(self.node_id.len()).ok()?;
        let ctrl_len = u16::try_from(self.ctrl_url.len()).ok()?;
        let token_len = u16::try_from(self.token.len()).ok()?;
        let total = CONFIG_HEADER_LEN
            .checked_add(self.node_id.len())?
            .checked_add(self.ctrl_url.len())?
            .checked_add(self.token.len())?;
        if total > CONFIG_BUDGET.max_bytes() {
            return None;
        }

        let mut out = alloc::vec::Vec::with_capacity(total);
        out.extend_from_slice(CONFIG_MAGIC);
        out.extend_from_slice(&node_len.to_le_bytes());
        out.extend_from_slice(&ctrl_len.to_le_bytes());
        out.extend_from_slice(&token_len.to_le_bytes());
        out.extend_from_slice(self.node_id.as_bytes());
        out.extend_from_slice(self.ctrl_url.as_bytes());
        out.extend_from_slice(self.token.as_bytes());
        Some(out)
    }

    fn decode(raw: &[u8]) -> Option<Self> {
        if raw.len() < CONFIG_HEADER_LEN || &raw[..4] != CONFIG_MAGIC {
            return None;
        }
        let node_len = u16::from_le_bytes([raw[4], raw[5]]) as usize;
        let ctrl_len = u16::from_le_bytes([raw[6], raw[7]]) as usize;
        let token_len = u16::from_le_bytes([raw[8], raw[9]]) as usize;
        if node_len > MAX_NODE_ID_LEN || ctrl_len > MAX_CTRL_URL_LEN || token_len > MAX_TOKEN_LEN {
            return None;
        }
        let node_end = CONFIG_HEADER_LEN.checked_add(node_len)?;
        let ctrl_end = node_end.checked_add(ctrl_len)?;
        let token_end = ctrl_end.checked_add(token_len)?;
        if token_end != raw.len() {
            return None;
        }
        Some(Self {
            node_id: String::from(core::str::from_utf8(&raw[CONFIG_HEADER_LEN..node_end]).ok()?),
            ctrl_url: String::from(core::str::from_utf8(&raw[node_end..ctrl_end]).ok()?),
            token: String::from(core::str::from_utf8(&raw[ctrl_end..token_end]).ok()?),
        })
    }
}

#[derive(Debug)]
pub enum AgentConfigError {
    Persistence,
    InvalidValue,
}

async fn load_config<B: ConfigBackend>(space: &ConfigSpace<B>) -> Result<AgentConfig, AgentConfigError> {
    match space.load().await {
        Ok(Some(snapshot)) => AgentConfig::decode(&snapshot.data).ok_or(AgentConfigError::InvalidValue),
        Ok(None) => Ok(AgentConfig::default()),
        Err(_) => Err(AgentConfigError::Persistence),
    }
}

/// A fallback identity for when no `node_id` has been persisted yet.
/// Deliberately opaque to `agent.rs`: the fallback is some stable,
/// device-unique string (a MAC-derived suffix on ESP), never how it was
/// derived.
pub trait DeviceIdentity {
    fn fallback_node_id(&self) -> String;
}

/// The device's `node_id` (contrat §1a): the ConfigSpace value if
/// provisioned, else `identity`'s temporary device-unique ID.
pub async fn node_id<B: ConfigBackend, I: DeviceIdentity>(space: &ConfigSpace<B>, identity: &I) -> String {
    if let Ok(config) = load_config(space).await
        && !config.node_id.is_empty()
    {
        return config.node_id;
    }
    identity.fallback_node_id()
}

pub async fn ctrl_url<B: ConfigBackend>(space: &ConfigSpace<B>) -> String {
    load_config(space).await.map(|c| c.ctrl_url).unwrap_or_default()
}

pub async fn token<B: ConfigBackend>(space: &ConfigSpace<B>) -> String {
    load_config(space).await.map(|c| c.token).unwrap_or_default()
}

/// Runtime prerequisite established by embewi-init. The API must never be
/// exposed without a durable Bearer token.
pub async fn is_provisioned<B: ConfigBackend>(space: &ConfigSpace<B>) -> bool {
    load_config(space)
        .await
        .is_ok_and(|config| !config.token.is_empty())
}

/// A source of random bytes for token generation. Opaque to `agent.rs`:
/// this crate doesn't know or care whether it's backed by a hardware RNG,
/// only that the bytes it gets are suitable for a Bearer token.
pub trait TokenEntropy {
    fn fill_random(&self, output: &mut [u8]);
}

/// 128-bit random token, hex-encoded (contrat §1a: "token vide → généré
/// aléatoirement par le device"). True randomness needs the RF subsystem up
/// (Wi-Fi) -- always the case here, since this is only ever called from the
/// HTTP config page, itself only reachable once on Wi-Fi.
fn generate_token<E: TokenEntropy>(entropy: &E) -> String {
    let mut bytes = [0u8; 16];
    entropy.fill_random(&mut bytes);
    let mut token = String::with_capacity(32);
    for b in bytes {
        let _ = write!(token, "{b:02x}");
    }
    token
}

/// Saves `node_id`/`ctrl_url`. `presented_token` empty means "keep the
/// existing token, or generate a fresh one if there isn't one yet" (contrat
/// §1a) -- it never clears an existing token, matching `POST /token`'s own
/// refusal of an empty value (§4: "on ne désactive pas l'auth par
/// rotation"). `http/mod.rs`'s save flow always calls this with an empty
/// `presented_token` (the form has no token field at all -- it's a
/// one-shot save, there's nothing to rotate to yet), so in practice this
/// only ever generates on first provisioning. `entropy` is only actually
/// read in that last case -- an existing or presented token never touches
/// it.
pub async fn save_identity<B: ConfigBackend, E: TokenEntropy>(
    space: &ConfigSpace<B>,
    node_id: &str,
    ctrl_url: &str,
    presented_token: &str,
    entropy: &E,
) -> Result<(), AgentConfigError> {
    let mut config = load_config(space).await?;
    config.node_id = String::from(node_id);
    config.ctrl_url = String::from(ctrl_url);
    if !presented_token.is_empty() {
        config.token = String::from(presented_token);
    } else if config.token.is_empty() {
        config.token = generate_token(entropy);
    }
    let encoded = config.encode().ok_or(AgentConfigError::InvalidValue)?;
    space.commit(&encoded).await.map_err(|_| AgentConfigError::Persistence)?;
    Ok(())
}

/// Common HTTP extractor; EmBewi applies its own constant-time token policy.
pub use iobewi_http::auth::Bearer;

/// Whether `presented` matches the stored Bearer token, compared in
/// constant time (contrat §1: "pas de fuite du token octet par octet").
/// `false` -- refusing every inbound call -- when no token has been
/// provisioned yet (contrat §1a).
pub async fn is_authorized<B: ConfigBackend>(space: &ConfigSpace<B>, presented: &str) -> bool {
    let token = token(space).await;
    if token.is_empty() {
        return false;
    }
    token.as_bytes().ct_eq(presented.as_bytes()).into()
}

/// Why a `POST /v1alpha1/token` call was refused (contrat §4).
pub enum RotateTokenError {
    /// Not the contract's stable vocabulary (§4b lists no named code for
    /// this one) -- the endpoint's own doc just says 400 + this message.
    InvalidLength,
    /// Contrat §4b: `nvs_write_failed`, HTTP 500. The commit is verified by
    /// reading the value straight back -- `Storage::set_string` itself is
    /// fire-and-forget, this is the only signal available that it actually
    /// landed, and the contract requires knowing before responding "rotated"
    /// ("l'écriture NVS est commitée avant la réponse").
    WriteFailed,
}

/// Rotates the Bearer token (contrat §4). Authorization (checking the
/// *current* token) is the caller's job, same as every other endpoint --
/// see [`is_authorized`]. An empty token is refused up front: rotation
/// never doubles as a way to disable auth (§4: "on ne désactive pas l'auth
/// par rotation").
pub async fn rotate_token<B: ConfigBackend>(space: &ConfigSpace<B>, new_token: &str) -> Result<(), RotateTokenError> {
    if !(8..=64).contains(&new_token.len()) {
        return Err(RotateTokenError::InvalidLength);
    }
    let mut config = load_config(space).await.map_err(|_| RotateTokenError::WriteFailed)?;
    config.token = String::from(new_token);
    let encoded = config.encode().ok_or(RotateTokenError::WriteFailed)?;
    space.commit(&encoded).await.map_err(|_| RotateTokenError::WriteFailed)?;
    if token(space).await == new_token {
        Ok(())
    } else {
        Err(RotateTokenError::WriteFailed)
    }
}

/// Contrat §2's device state machine. Drives both `GET /info`/`GET /health`
/// and the heartbeat's `state`/`ota_validated` (contrat §3/§5) -- one
/// source of truth instead of independently-guessed literals drifting
/// apart. `Ordering::Relaxed` throughout: riscv32imc has no atomic
/// read-modify-write (see `status.rs`'s identical pattern), and this value
/// is only ever overwritten, never updated in place, so a plain
/// store/load is enough.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    Booting = 0,
    PendingVerify = 1,
    Running = 2,
    Degraded = 3,
    Rollback = 4,
    Failed = 5,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Booting => "booting",
            State::PendingVerify => "pending_verify",
            State::Running => "running",
            State::Degraded => "degraded",
            State::Rollback => "rollback",
            State::Failed => "failed",
        }
    }

    fn from_byte(byte: u8) -> Self {
        match byte {
            1 => State::PendingVerify,
            2 => State::Running,
            3 => State::Degraded,
            4 => State::Rollback,
            5 => State::Failed,
            _ => State::Booting,
        }
    }
}

static STATE: AtomicU8 = AtomicU8::new(State::Booting as u8);

pub fn set_state(state: State) {
    STATE.store(state as u8, Ordering::Relaxed);
}

pub fn state() -> State {
    State::from_byte(STATE.load(Ordering::Relaxed))
}

#[derive(Serialize)]
struct Firmware {
    name: &'static str,
    version: &'static str,
    digest: String,
}

/// Contrat §4's `staged` object: `{"state":"none"}` alone when nothing's
/// staged (`slot`/`digest`/`deployment_id` omitted, not sent empty), the
/// full object once `/ota/write` has landed something.
/// The bootloader's `otadata` (EWBT) as read from flash: newest entry's slot,
/// sequence and state. Independent of `active_slot` (what the MMU is running)
/// so a rollback shows up as the two disagreeing.
#[derive(Serialize)]
struct BootInfo {
    slot: &'static str,
    seq: u32,
    state: &'static str,
}

#[derive(Serialize)]
struct StagedInfo {
    state: &'static str,
    #[serde(skip_serializing_if = "String::is_empty")]
    slot: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    digest: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    deployment_id: String,
}

/// Portable view of the bootloader's `otadata` (EWBT) entry: newest entry's
/// slot, sequence and state. Belongs to the agent domain, not to any ESP
/// OTA storage type -- see [`BootInfoSource`].
pub struct BootSnapshot {
    pub slot: &'static str,
    pub seq: u32,
    pub state: &'static str,
}

/// The boot-time facts `GET /info` needs (contrat §4's `active_slot`/
/// `boot`), independent of how they're actually read off flash. Keeps
/// `info()` from depending on `SharedFlash` or any other ESP OTA storage
/// type -- the platform adapter lives at the composition root instead.
#[allow(async_fn_in_trait)]
pub trait BootInfoSource {
    async fn active_slot(&self) -> String;
    async fn boot_info(&self) -> BootSnapshot;
}

/// Platform facts `GET /info` reports as-is (contrat §4's `chip`/
/// `ram_size`), independent of how they're actually read. `ram_size` is the
/// chip's total DRAM from its linker memory map -- a hardware constant, not
/// free/configured heap (`heartbeat.rs`'s `heap_free` already covers that,
/// a much smaller, firmware-configured subset of this).
pub trait DeviceMetadata {
    fn chip_name(&self) -> &'static str;
    fn ram_size(&self) -> u32;
}

/// `GET /v1alpha1/info` response body (contrat §4).
#[derive(Serialize)]
pub struct Info {
    node_id: String,
    api_versions: &'static [&'static str],
    chip: &'static str,
    ram_size: u32,
    partition_layout: &'static str,
    active_slot: String,
    boot: BootInfo,
    firmware: Firmware,
    staged: StagedInfo,
    state: &'static str,
    config_generation: u64,
    app_port: u16,
}

pub async fn info<AB: ConfigBackend, APPB: ConfigBackend, RB: ConfigBackend, B: BootInfoSource, M: DeviceMetadata, I: DeviceIdentity>(
    boot: &B,
    metadata: &M,
    identity: &I,
    agent_config: &ConfigSpace<AB>,
    app_config: &ConfigSpace<APPB>,
    runtime_config: &crate::runtime_config::RuntimeConfig<RB>,
    ota_config: &crate::ota::OtaConfigSpace,
) -> Info {
    let config_generation = runtime_config.generation().await;
    let app_port = crate::app_config::port(app_config).await;
    let staged = crate::ota::staged(ota_config).await;
    Info {
        node_id: node_id(agent_config, identity).await,
        api_versions: API_VERSIONS,
        chip: metadata.chip_name(),
        ram_size: metadata.ram_size(),
        partition_layout: crate::ota::PARTITION_LAYOUT,
        active_slot: boot.active_slot().await,
        boot: {
            let boot = boot.boot_info().await;
            BootInfo { slot: boot.slot, seq: boot.seq, state: boot.state }
        },
        firmware: Firmware {
            name: FW_NAME,
            version: FW_VERSION,
            digest: crate::ota::active_digest(ota_config).await,
        },
        staged: StagedInfo {
            state: staged.stage.as_str(),
            slot: staged.slot,
            digest: staged.digest,
            deployment_id: staged.deployment_id,
        },
        state: state().as_str(),
        config_generation,
        app_port,
    }
}

#[derive(Serialize)]
struct Checks {
    app: &'static str,
    sensors: &'static str,
    storage: &'static str,
}

/// `GET /v1alpha1/health` response body (contrat §4) -- local health, not
/// just network reachability.
#[derive(Serialize)]
pub struct Health {
    status: &'static str,
    state: &'static str,
    checks: Checks,
}

/// "Is the storage this agent's identity/config lives on currently
/// considered sound?" -- a runtime diagnostic of the concrete backend, not
/// a persistence operation every `ConfigSpace` backend must offer (that's
/// `ConfigBackend`'s `load`/`commit`/`clear`/`capacity`).
pub trait StorageHealth {
    fn is_healthy(&self) -> bool;
}

pub async fn health<H: StorageHealth>(storage: &H) -> Health {
    // Last known NVS health: the canary round-trip (contrat's own C
    // reference does the same test, for the same reason -- staged OTA
    // state, the token and McuConfigMap all live there) ran at boot and
    // runs again as the `pending_verify` gate, and any NVS error since
    // then has cleared it. A health probe never writes flash: polled at
    // 1 Hz it would mean ~170k NVS mutations a day.
    let storage_ok = storage.is_healthy();
    // No separate workload process or sensors on this agent (single
    // binary) -- vacuously true, same reasoning the reference
    // implementation uses for its demo apps that have no sensors either.
    let app_ok = true;
    let sensors_ok = true;

    let ok = |b: bool| if b { "ok" } else { "fail" };
    Health {
        status: ok(storage_ok && app_ok && sensors_ok),
        state: state().as_str(),
        checks: Checks {
            app: ok(app_ok),
            sensors: ok(sensors_ok),
            storage: ok(storage_ok),
        },
    }
}
