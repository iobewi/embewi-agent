//! Embewi's OTA adapter (contrat §3/§4/§6): the Core streams a raw `.bin`
//! into whichever `ota_0`/`ota_1` slot isn't currently booted.
//!
//! IOBEWI OTA owns the transaction state machine and the OTA metadata schema.
//! `iobewi-esp-ota-boot` owns the EWBT boot state machine and ESP image
//! validation. `iobewi-esp-ota` locates ESP partitions, executes EWBT flash
//! writes with readback and provides the NOR-flash artifact backend. This
//! module is the remaining application-facing facade over that state: the
//! bootstrap lifecycle, read-only OTA metadata, and the boot-time self-check
//! gate. The `/ota/prepare`, `/ota/write` and `/ota/activate` HTTP surface
//! composes `iobewi_ota::service`/`iobewi_esp_ota` directly in
//! `supervisor.rs` (`AgentOtaBackend`/`EspFactoryOta`) instead of going
//! through a wrapper here.
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

use crate::agent;
use iobewi_ota::metadata::{Metadata as OtaMetadata, PendingCheckAction};
pub use iobewi_ota::metadata::{MetadataError as OtaMetadataError, Stage, Staged};
use iobewi_ota::config_space::{ConfigSpaceBootstrapStore, ConfigSpaceMetadataStore};
use iobewi_ota::service::boot::BootOps;

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

/// Reported verbatim in `GET /v1alpha1/info`'s `partition_layout` field
/// (contrat §4) -- `/ota/prepare`'s own compatibility check now reads this
/// same constant directly from `iobewi_esp_ota` at the composition root
/// (`supervisor.rs`); this re-export stays only because `agent::info()`
/// still needs a value here, not because it's this facade's own data.
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

/// Contrat §3: how long a `pending_verify` self-check gets before this
/// device forces its own reset -- unconfirmed past this, the bootloader's
/// own rollback takes over on the next boot. Same value `firmware-c` uses
/// (`EMBEWI_PENDING_DEADLINE_MS`).
const SELFCHECK_DEADLINE: Duration = Duration::from_millis(iobewi_ota::metadata::PENDING_VERIFY_TIMEOUT_MS);
/// How long the anti-freeze watchdog gets before it force-resets the
/// device. Longer than `SELFCHECK_DEADLINE` so the graceful, logged
/// software timeout in `selfcheck_task` fires first in the ordinary case;
/// this is the hardware backstop for when even that doesn't run -- a hang
/// before the self-check's own `select` is ever reached, or one inside the
/// embassy executor itself, neither of which a purely software deadline
/// (which depends on that same executor) can catch. Arming the actual
/// hardware watchdog is a platform mechanism the composition root owns
/// (`iobewi_esp_ota::service::arm_watchdog_ms`); this constant is the
/// Embewi policy of *how long*, not *how*.
pub const BOOT_WATCHDOG_DEADLINE_MS: u64 = 20_000;

/// A platform reset effect. `iobewi_ota::service::boot::BootOps`
/// deliberately has no notion of reboot/reset (see its own docs); Embewi's
/// boot policy needs exactly these two, so this crate owns the port and
/// the ESP composition root supplies the implementation.
#[allow(async_fn_in_trait)]
pub trait BootReset {
    /// A brief delay, then an unconditional reset -- gives the log line
    /// that preceded the call a chance to actually flush first.
    async fn reset_for_rollback(&self) -> !;
    /// Immediate reset, no delay.
    fn reset_now(&self) -> !;
}


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

/// Persists IOBEWI OTA's validation result after the image is confirmed.
/// If a write fails the `activating` record is kept and the agent goes
/// `Degraded`: the image is valid and stays so (never rolled back over
/// bookkeeping), and the next boot -- bootloader `Valid`, same slot, still
/// `activating` -- completes this promotion.
pub async fn confirm_pending<OB: ConfigBackend, B: BootOps, R: BootReset>(
    boot: &B,
    reset: &R,
    ota_config: &OtaConfigSpace<OB>,
) {
    // Fault injection remains a firmware-only validation hook. This loop
    // starves the executor so only the hardware watchdog can recover.
    if cfg!(feature = "fault-injection-freeze") {
        warn!("ota: [fault-injection-freeze] spinning forever, only the hardware watchdog can save this boot");
        loop { core::hint::spin_loop(); }
    }
    match iobewi_ota::service::boot::pending_check(boot, SELFCHECK_DEADLINE).await {
        PendingCheckAction::Confirm => {
            if cfg!(feature = "fault-injection") {
                warn!("ota: [fault-injection] self-check passed, resetting BEFORE confirm to exercise rollback");
                reset.reset_for_rollback().await;
            }
            match iobewi_ota::service::boot::confirm_pending(&ConfigSpaceMetadataStore(ota_config), boot).await {
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
                    reset.reset_for_rollback().await;
                }
            }
        }
        PendingCheckAction::Reject => {
            iobewi_ota::service::boot::reject_pending(boot).await;
            warn!("ota: self-check failed, marking image invalid and rebooting for rollback");
            reset.reset_for_rollback().await;
        }
        PendingCheckAction::Reset => {
            warn!("ota: self-check deadline exceeded, forcing a reset (bootloader will roll back)");
            reset.reset_now();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootDisposition { Stable, PendingVerify }

pub async fn on_boot<OB: ConfigBackend, B: BootOps, R: BootReset>(
    boot: &B,
    reset: &R,
    ota_config: &OtaConfigSpace<OB>,
) -> BootDisposition {
    use iobewi_ota::service::boot::BootStatus;
    let result = iobewi_ota::service::boot::on_boot(&ConfigSpaceMetadataStore(ota_config), boot).await;
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
            reset.reset_for_rollback().await;
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

pub async fn reject_pending<B: BootOps, R: BootReset>(boot: &B, reset: &R) -> ! {
    iobewi_ota::service::boot::reject_pending(boot).await;
    reset.reset_for_rollback().await
}
