//! Embewi's OTA adapter (contrat §3/§4/§6): the Core streams a raw `.bin`
//! into whichever `ota_0`/`ota_1` slot isn't currently booted.
//!
//! IOBEWI OTA owns the transaction state machine and the OTA metadata schema.
//! `iobewi-esp-ota-boot` owns the EWBT boot state machine and ESP image
//! validation. `iobewi-esp-ota` locates ESP partitions, executes EWBT flash
//! writes with readback and provides the NOR-flash artifact backend. Here the
//! application binds ConfigSpace,
//! the HTTP transport, watchdog policy and self-check gate.
//!
//! Mirrors `firmware-c`'s `embewi_ota.c`/`embewi_selfcheck.c` state machine
//! (same `stage`/`slot`/`digest`/`deployment_id`/`size` staged-NVS layout),
//! reimplemented against embassy tasks instead of ESP-IDF's C one and
//! FreeRTOS tasks -- the resume-decision and reconciliation *logic* itself
//! now lives in `iobewi-ota`, not reimplemented here.
//!
//! Staged state is persisted to NVS (not just kept in RAM) because, unlike
//! a Core restart, the reconcile in contrat §6 also has to survive *this*
//! device rebooting between `/ota/write` and `/ota/activate` -- the write
//! session itself (running SHA-256, byte offset) doesn't need to, since a
//! reboot there always restarts the Core from `start=0` (contrat's own
//! `[RÉSERVE]` on inter-reboot write resume).

use alloc::string::String;

use embassy_time::Duration;
use iobewi_config_space::{Budget, ConfigBackend, ConfigSpace};
use log::{info, warn};
pub use iobewi_ota::http::{PrepareRequest, PrepareResponse};

use crate::agent;
use iobewi_ota::metadata::{
    Metadata as OtaMetadata,
    format_digest, parse_digest, PendingCheckAction,
};
pub use iobewi_ota::metadata::{MetadataError as OtaMetadataError, SessionParams, Stage, Staged};
use iobewi_esp_ota::otadata;
use iobewi_esp_ota::shared_flash as platform_ota;
use iobewi_esp_ota::service::EspBoot;
use iobewi_ota::config_space::{ConfigSpaceBootstrapStore, ConfigSpaceMetadataStore};

use iobewi_esp_config_space::NvsConfigBackend;

pub use iobewi_ota::bootstrap::{BootstrapError, BootstrapState};
pub const BOOTSTRAP_CONFIG_BUDGET: Budget =
    Budget::new(iobewi_ota::bootstrap::MAX_BYTES);
pub type BootstrapConfigSpace<B> = ConfigSpace<B>;
pub type OtaConfigSpace<B> = ConfigSpace<B>;

pub async fn bootstrap_state<B: ConfigBackend>(
    space: &ConfigSpace<B>,
) -> Result<BootstrapState, BootstrapError> {
    iobewi_ota::bootstrap::state(&ConfigSpaceBootstrapStore(space)).await
}

pub async fn begin_provisioning<B: ConfigBackend>(
    space: &ConfigSpace<B>,
) -> Result<(), BootstrapError> {
    iobewi_ota::bootstrap::begin_provisioning(&ConfigSpaceBootstrapStore(space)).await
}

pub async fn ready_for_agent<B: ConfigBackend>(
    space: &ConfigSpace<B>,
) -> Result<(), BootstrapError> {
    iobewi_ota::bootstrap::ready_for_agent(&ConfigSpaceBootstrapStore(space)).await
}

pub async fn production<B: ConfigBackend>(
    space: &ConfigSpace<B>,
) -> Result<(), BootstrapError> {
    iobewi_ota::bootstrap::production(&ConfigSpaceBootstrapStore(space)).await
}
use iobewi_esp_flash::SharedFlash;

/// Contrat §4: `POST /ota/prepare`'s `partition_layout` field must match
/// this exactly, or the write is refused before a single byte transfers.
/// Bump only if `partitions.csv`'s slot layout ever changes shape.
pub use iobewi_esp_ota::PARTITION_LAYOUT;

pub const CONFIG_BUDGET: Budget = Budget::new(OtaMetadata::MAX_BYTES);

/// Metadata for the first production agent image preloaded by the factory
/// ESP Web Tools image into the inactive OTA slot.
#[derive(Clone, Copy)]
pub struct PreloadedAgent {
    pub size: u32,
    pub digest: &'static str,
    pub deployment_id: &'static str,
}

#[derive(Debug)]
pub enum PreloadedAgentError {
    BadDigest,
    NoTarget,
    TooLarge,
    Flash,
    DigestMismatch,
    Metadata(OtaMetadataError),
}


/// Contrat §3: how long a `pending_verify` self-check gets before this
/// device forces its own reset -- unconfirmed past this, the bootloader's
/// own rollback takes over on the next boot. Same value `firmware-c` uses
/// (`EMBEWI_PENDING_DEADLINE_MS`).
const SELFCHECK_DEADLINE: Duration = Duration::from_millis(iobewi_ota::metadata::PENDING_VERIFY_TIMEOUT_MS);
/// How long the anti-freeze watchdog (see [`arm_boot_watchdog`]) gets before
/// it force-resets the device. Longer than `SELFCHECK_DEADLINE` so the
/// graceful, logged software timeout in `selfcheck_task` fires first in the
/// ordinary case; this is the hardware backstop for when even that doesn't
/// run -- a hang before the self-check's own `select` is ever reached, or
/// one inside the embassy executor itself, neither of which a purely
/// software deadline (which depends on that same executor) can catch.
const WATCHDOG_DEADLINE_MS: u64 = 20_000;


async fn load_metadata<OB: ConfigBackend>(space: &OtaConfigSpace<OB>) -> Result<OtaMetadata, OtaMetadataError> {
    iobewi_ota::metadata::load_metadata(&ConfigSpaceMetadataStore(space)).await
}

pub async fn staged<OB: ConfigBackend>(space: &OtaConfigSpace<OB>) -> Staged {
    match load_metadata(space).await {
        Ok(metadata) => metadata.staged,
        Err(e) => {
            warn!("ota: metadata load failed: {e:?}");
            Staged::default()
        }
    }
}

pub async fn clear_staged<OB: ConfigBackend>(space: &OtaConfigSpace<OB>) -> Result<(), OtaMetadataError> {
    iobewi_ota::metadata::clear_staged(&ConfigSpaceMetadataStore(space)).await
}

/// Digest of the currently-running, validated firmware.
pub async fn active_digest<OB: ConfigBackend>(space: &OtaConfigSpace<OB>) -> String {
    load_metadata(space)
        .await
        .map(|metadata| metadata.active_digest)
        .unwrap_or_default()
}

/// The deployment_id of the currently-running, validated firmware.
pub async fn active_deployment_id<OB: ConfigBackend>(space: &OtaConfigSpace<OB>) -> String {
    load_metadata(space)
        .await
        .map(|metadata| metadata.active_deployment_id)
        .unwrap_or_default()
}

/// The partition actually booted, which can differ from the latest EWBT
/// entry after a fallback from an image with an invalid app header.
pub async fn active_slot(flash: &SharedFlash) -> String {
    String::from(platform_ota::active_slot(flash).await)
}

pub type BootEntry = otadata::BootEntry;

/// Raw bootloader state, independent of the agent's own status.
pub async fn boot_info(flash: &SharedFlash) -> BootEntry {
    platform_ota::boot_info(flash).await
}

/// Verifies the already-programmed inactive slot and publishes it as a
/// normal IOBEWI OTA staged transaction. No alternate OTA/write path exists:
/// factory flashing merely placed the bytes there ahead of time.
pub async fn stage_preloaded_agent<OB: ConfigBackend>(
    flash: &SharedFlash,
    ota_config: &OtaConfigSpace<OB>,
    image: PreloadedAgent,
) -> Result<&'static str, PreloadedAgentError> {
    let expected = parse_digest(image.digest).ok_or(PreloadedAgentError::BadDigest)?;

    let (slot, computed) = platform_ota::hash_preloaded(flash, image.size).await.map_err(|e| match e {
        platform_ota::PreloadedError::NoTarget => PreloadedAgentError::NoTarget,
        platform_ota::PreloadedError::TooLarge => PreloadedAgentError::TooLarge,
        platform_ota::PreloadedError::Flash => PreloadedAgentError::Flash,
    })?;

    if computed != expected {
        return Err(PreloadedAgentError::DigestMismatch);
    }

    iobewi_ota::service::publish(
        &ConfigSpaceMetadataStore(ota_config), String::from(image.deployment_id),
        iobewi_ota::Committed { size: u64::from(image.size), digest: expected },
        String::from(slot.as_str()),
    )
        .await
        .map_err(PreloadedAgentError::Metadata)?;

    Ok(slot.as_str())
}

/// Validates compat *before* a single byte transfers (contrat §3: "un
/// binaire esp32-s3 flashé sur esp32 ne boote pas").
pub async fn prepare<OB: ConfigBackend>(flash: &SharedFlash, ota_config: &OtaConfigSpace<OB>, req: &PrepareRequest) -> PrepareResponse {
    match iobewi_ota::service::prepare(
        &ConfigSpaceMetadataStore(ota_config), &EspBoot(flash), &req.chip, &req.partition_layout,
        esp_metadata_generated::chip_pretty!(), PARTITION_LAYOUT, u64::from(req.size),
    ).await {
        Ok(target) => PrepareResponse::accept(target),
        Err(reason) => PrepareResponse::refuse(reason),
    }
}

/// IOBEWI OTA owns the session and the publish-on-verified-finish rule.
/// The ESP adapter supplies only the writer that programs the inactive slot.
static UPLOAD: iobewi_ota::service::upload::UploadManager<iobewi_esp_ota::service::EspUploadWriter> =
    iobewi_ota::service::upload::UploadManager::new();

pub async fn write_in_progress() -> bool { UPLOAD.in_progress().await }
pub async fn write_written() -> u32 { UPLOAD.written().await }
pub async fn write_received() -> u32 { UPLOAD.received().await }
pub async fn write_params_match(params: &SessionParams) -> bool { UPLOAD.params_match(params).await }

pub use iobewi_ota::ResumePlan as Plan;

pub fn write_plan(has_range: bool, start: u32, in_progress: bool, written: u32) -> Plan {
    iobewi_ota::resume_plan(has_range, u64::from(start), in_progress, u64::from(written))
}

pub fn write_is_final(has_range: bool, end: u32, total: u32) -> bool {
    iobewi_ota::is_complete(has_range, u64::from(end), u64::from(total))
}

pub enum BeginError {
    Busy,
    TooLarge,
    Conflict,
    Storage(OtaMetadataError),
}

pub async fn write_begin<OB: ConfigBackend>(
    flash: &'static SharedFlash,
    ota_config: &OtaConfigSpace<OB>,
    params: SessionParams,
) -> Result<(), BeginError> {
    // Target selection releases the physical flash lock before the portable
    // service touches ConfigSpace: both capabilities share that same lock.
    let target = platform_ota::write_target(flash).await.map_err(|_| BeginError::Busy)?;
    UPLOAD.begin(
        &ConfigSpaceMetadataStore(ota_config), params, target.size as u64,
        iobewi_esp_ota::service::EspUploadWriter::new(flash, target),
    ).await.map_err(|error| match error {
        iobewi_ota::service::upload::StartError::TooLarge => BeginError::TooLarge,
        iobewi_ota::service::upload::StartError::Busy => BeginError::Busy,
        iobewi_ota::service::upload::StartError::Conflict => BeginError::Conflict,
        iobewi_ota::service::upload::StartError::Storage(error) => BeginError::Storage(error),
    })
}

pub async fn write_chunk(data: &[u8]) -> bool { UPLOAD.chunk(data).await }

pub struct WriteFinishOk {
    pub written: u32,
    pub digest: String,
}

pub enum WriteFinishError {
    NotWriting,
    DigestMismatch,
    Incomplete,
    Storage(OtaMetadataError),
}

pub async fn write_finish<OB: ConfigBackend>(ota_config: &OtaConfigSpace<OB>) -> Result<WriteFinishOk, WriteFinishError> {
    let result = UPLOAD.finish(&ConfigSpaceMetadataStore(ota_config)).await.map_err(|error| match error {
        iobewi_ota::service::upload::FinishError::NotWriting => WriteFinishError::NotWriting,
        iobewi_ota::service::upload::FinishError::DigestMismatch(computed) => {
            warn!("ota: digest mismatch, calculated={}", format_digest(&computed));
            WriteFinishError::DigestMismatch
        }
        iobewi_ota::service::upload::FinishError::Incomplete { durable } => {
            warn!("ota: incomplete session at {durable} bytes");
            WriteFinishError::Incomplete
        }
        iobewi_ota::service::upload::FinishError::Backend => WriteFinishError::Incomplete,
        iobewi_ota::service::upload::FinishError::Storage(error) => WriteFinishError::Storage(error),
    })?;
    info!(
        "ota: write OK {} octets ({} secteurs programmés, {} blocs erase de {} KiB) en {}ms slot={} -> staged=written",
        result.written, result.stats.sectors_flushed, result.stats.erase_batches,
        result.stats.erase_batch_kib, result.elapsed_ms, result.slot,
    );
    Ok(WriteFinishOk { written: result.written, digest: result.digest })
}

/// `POST /v1alpha1/ota/activate` (contrat §4): points the bootloader at the
/// staged slot and arms `OtaImageState::New` (which it promotes to
/// `PendingVerify` on the next boot). Reads the target slot from the persisted
/// ConfigSpace `staged` state, not the in-RAM write session -- matches `firmware-c`'s
/// own fallback ("Reprise après reboot de l'agent entre write et
/// activate"), and works identically whether or not this device rebooted
/// since `/ota/write` finished.
pub async fn activate<OB: ConfigBackend>(flash: &SharedFlash, ota_config: &OtaConfigSpace<OB>, deployment_id: &str) -> Result<String, ActivateError> {
    let slot = iobewi_ota::service::activate_staged(
        &ConfigSpaceMetadataStore(ota_config), &EspBoot(flash), deployment_id,
    ).await.map_err(|error| match error {
        iobewi_ota::service::ActivateError::NotStaged => ActivateError::NotStaged,
        iobewi_ota::service::ActivateError::DeploymentMismatch => ActivateError::DeploymentMismatch,
        iobewi_ota::service::ActivateError::Storage(error) => ActivateError::Storage(error),
    })?;
    info!("ota: activate dep={deployment_id} -> slot={slot} prêt, reboot imminent");
    Ok(slot)
}

pub enum ActivateError {
    /// Nothing staged, or `otadata` couldn't be updated (`409 not_staged`,
    /// as before).
    NotStaged,
    /// The staged image belongs to another deployment (`409`).
    DeploymentMismatch,
    /// The staged record couldn't be persisted; nothing was activated.
    Storage(OtaMetadataError),
}

/// Persists IOBEWI OTA's validation result after the image is confirmed.
/// If a write fails the `activating` record is kept and the agent goes
/// `Degraded`: the image is valid and stays so (never rolled back over
/// bookkeeping), and the next boot -- bootloader `Valid`, same slot, still
/// `activating` -- completes this promotion.
// The watchdog must be armed only after TIMG0 has been initialized: the first
// TimerGroup::new resets the peripheral block and would discard an earlier arm.

pub fn arm_boot_watchdog() {
    iobewi_esp_ota::service::arm_watchdog_ms(WATCHDOG_DEADLINE_MS);
}

pub async fn confirm_pending<OB: ConfigBackend>(
    flash: &'static SharedFlash,
    nvs_backend: &'static NvsConfigBackend,
    ota_config: &'static OtaConfigSpace<OB>,
) {
    // Fault injection remains a firmware-only validation hook. This loop
    // starves the executor so only the hardware watchdog can recover.
    if cfg!(feature = "fault-injection-freeze") {
        warn!("ota: [fault-injection-freeze] spinning forever, only the hardware watchdog can save this boot");
        loop { core::hint::spin_loop(); }
    }
    let platform = iobewi_esp_ota::service::EspBootRuntime { flash, nvs: nvs_backend };
    match iobewi_ota::service::boot::pending_check(&platform, SELFCHECK_DEADLINE).await {
        PendingCheckAction::Confirm => {
            if cfg!(feature = "fault-injection") {
                warn!("ota: [fault-injection] self-check passed, resetting BEFORE confirm to exercise rollback");
                iobewi_esp_ota::service::reset_for_rollback().await;
            }
            match iobewi_ota::service::boot::confirm_pending(&ConfigSpaceMetadataStore(ota_config), &platform).await {
                iobewi_ota::service::boot::Confirmation::Valid => {
                    agent::set_state(agent::State::Running);
                    info!("ota: validation done");
                }
                iobewi_ota::service::boot::Confirmation::Degraded => {
                    warn!("ota: confirmed image metadata couldn't be persisted; will retry at next boot");
                    agent::set_state(agent::State::Degraded);
                }
                iobewi_ota::service::boot::Confirmation::ResetRequired => {
                    warn!("ota: couldn't confirm the running image, rolling back");
                    iobewi_esp_ota::service::reset_for_rollback().await;
                }
            }
        }
        PendingCheckAction::Reject => {
            iobewi_ota::service::boot::reject_pending(&platform).await;
            warn!("ota: self-check failed, marking image invalid and rebooting for rollback");
            iobewi_esp_ota::service::reset_for_rollback().await;
        }
        PendingCheckAction::Reset => {
            warn!("ota: self-check deadline exceeded, forcing a reset (bootloader will roll back)");
            iobewi_esp_ota::service::reset_now();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootDisposition { Stable, PendingVerify }

pub async fn on_boot<OB: ConfigBackend>(
    flash: &'static SharedFlash,
    nvs_backend: &'static NvsConfigBackend,
    ota_config: &'static OtaConfigSpace<OB>,
) -> BootDisposition {
    use iobewi_ota::service::boot::BootStatus;
    let platform = iobewi_esp_ota::service::EspBootRuntime { flash, nvs: nvs_backend };
    let result = iobewi_ota::service::boot::on_boot(&ConfigSpaceMetadataStore(ota_config), &platform).await;
    info!("ota: boot slot={:?} image={:?} -> {:?}", result.slot, result.image, result.action);
    match result.status {
        BootStatus::PendingVerify => {
            agent::set_state(agent::State::PendingVerify);
            warn!("ota: image is PENDING_VERIFY, application confirmation required");
            BootDisposition::PendingVerify
        }
        BootStatus::Rollback => {
            agent::set_state(agent::State::Rollback);
            warn!("ota: PENDING_VERIFY image not accounted for by staged record, rolling back");
            iobewi_esp_ota::service::reset_for_rollback().await;
        }
        BootStatus::Degraded => {
            agent::set_state(agent::State::Degraded);
            warn!("ota: confirmed image bookkeeping failed, will retry at next boot");
            BootDisposition::Stable
        }
        BootStatus::Stable => {
            if result.storage_healthy == Some(false) {
                warn!("ota: boot NVS self-check failed, /health will report storage=fail");
            }
            agent::set_state(agent::State::Running);
            BootDisposition::Stable
        }
    }
}

pub async fn reject_pending(flash: &'static SharedFlash) -> ! {
    iobewi_esp_ota::service::reject_and_reset(flash).await
}
