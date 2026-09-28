//! Embewi's OTA adapter (contrat §3/§4/§6): the Core streams a raw `.bin`
//! into whichever `ota_0`/`ota_1` slot isn't currently booted.
//!
//! FiBeWI owns the transaction and EWBT state machines. `espbewi-ota`
//! locates ESP partitions, executes EWBT flash writes with readback, and
//! provides the NOR-flash artifact backend. Here the application owns its
//! ConfigSpace schema, HTTP contract, watchdog policy and self-check gate.
//!
//! Mirrors `firmware-c`'s `embewi_ota.c`/`embewi_selfcheck.c` state machine
//! (same `stage`/`slot`/`digest`/`deployment_id`/`size` staged-NVS layout),
//! reimplemented against embassy tasks instead of ESP-IDF's C one and
//! FreeRTOS tasks -- the resume-decision and reconciliation *logic* itself
//! now lives in `fibewi`, not reimplemented here.
//!
//! Staged state is persisted to NVS (not just kept in RAM) because, unlike
//! a Core restart, the reconcile in contrat §6 also has to survive *this*
//! device rebooting between `/ota/write` and `/ota/activate` -- the write
//! session itself (running SHA-256, byte offset) doesn't need to, since a
//! reboot there always restarts the Core from `start=0` (contrat's own
//! `[RÉSERVE]` on inter-reboot write resume).

use alloc::boxed::Box;
use alloc::string::String;

use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant, Timer};
use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use config_space_manager::{Budget, ConfigSpace};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::agent;
use fibewi::{Action, BackendOutcome, TransactionState};
use fibewi::embewi::{
    Metadata as OtaMetadata, Transaction as OtaTransaction,
    format_digest, parse_digest, staged_from_transaction, transaction_from_staged,
};
pub use fibewi::embewi::{MetadataError as OtaMetadataError, Stage, Staged};
use espbewi_ota::{AppPartition, AppSlot, EspArtifactStorage, erase_partition_range, otadata};

use espbewi_config_space::NvsConfigBackend;
use espbewi_flash::{EspFlash, SharedFlash};

/// Contrat §4: `POST /ota/prepare`'s `partition_layout` field must match
/// this exactly, or the write is refused before a single byte transfers.
/// Bump only if `partitions.csv`'s slot layout ever changes shape.
pub const PARTITION_LAYOUT: &str = "embewi-ab-v1";

pub const CONFIG_BUDGET: Budget = Budget::new(OtaMetadata::MAX_BYTES);
pub type OtaConfigSpace = ConfigSpace<NvsConfigBackend>;

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
const SELFCHECK_DEADLINE: Duration = Duration::from_secs(15);
/// How long the anti-freeze watchdog (see [`arm_boot_watchdog`]) gets before
/// it force-resets the device. Longer than `SELFCHECK_DEADLINE` so the
/// graceful, logged software timeout in `selfcheck_task` fires first in the
/// ordinary case; this is the hardware backstop for when even that doesn't
/// run -- a hang before the self-check's own `select` is ever reached, or
/// one inside the embassy executor itself, neither of which a purely
/// software deadline (which depends on that same executor) can catch.
const WATCHDOG_DEADLINE_MS: u64 = 20_000;


/// Keep the partition-table scratch area off the Embassy task stack.
fn table_buffer() -> Box<otadata::TableBuffer> {
    Box::new([0u8; espbewi_ota::PARTITION_TABLE_BUFFER_SIZE])
}

// Each operation holds the shared physical flash lock only while using it.
// ConfigSpace takes the same non-reentrant lock, so its calls stay outside.
fn write_target_locked(flash: &mut EspFlash) -> Option<AppPartition> {
    otadata::write_target(flash.storage(), &mut table_buffer()).ok()
}

async fn otadata_confirm(flash: &SharedFlash) -> Result<(), otadata::Error> {
    let mut guard = flash.lock().await;
    otadata::confirm(guard.storage(), &mut table_buffer())
}

async fn otadata_reject(flash: &SharedFlash) -> Result<(), otadata::Error> {
    let mut guard = flash.lock().await;
    otadata::reject(guard.storage(), &mut table_buffer())
}

async fn otadata_activate(flash: &SharedFlash, target: AppSlot) -> Result<(), otadata::Error> {
    let mut guard = flash.lock().await;
    otadata::activate(guard.storage(), &mut table_buffer(), target)
}

async fn load_metadata(space: &OtaConfigSpace) -> Result<OtaMetadata, OtaMetadataError> {
    match space.load().await {
        Ok(Some(snapshot)) => OtaMetadata::decode(&snapshot.data),
        Ok(None) => Ok(OtaMetadata::default()),
        Err(_) => Err(OtaMetadataError::Persistence),
    }
}

async fn save_metadata(space: &OtaConfigSpace, metadata: &OtaMetadata) -> Result<(), OtaMetadataError> {
    let encoded = metadata.encode()?;
    space.commit(&encoded)
        .await
        .map(|_| ())
        .map_err(|_| OtaMetadataError::Persistence)
}

pub async fn staged(space: &OtaConfigSpace) -> Staged {
    match load_metadata(space).await {
        Ok(metadata) => metadata.staged,
        Err(e) => {
            warn!("ota: metadata load failed: {e:?}");
            Staged::default()
        }
    }
}

async fn save_staged(
    space: &OtaConfigSpace,
    stage: Stage,
    slot: &str,
    digest: &str,
    deployment_id: &str,
    size: u32,
) -> Result<(), OtaMetadataError> {
    let mut metadata = load_metadata(space).await?;
    metadata.set_staged(Staged {
        stage,
        slot: String::from(slot),
        digest: String::from(digest),
        deployment_id: String::from(deployment_id),
        size,
    });
    save_metadata(space, &metadata).await
}

pub async fn clear_staged(space: &OtaConfigSpace) -> Result<(), OtaMetadataError> {
    save_staged(space, Stage::None, "", "", "", 0).await
}

struct MemoryTransactionMetadata {
    record: Option<OtaTransaction>,
}

impl fibewi::TransactionMetadata for MemoryTransactionMetadata {
    type Error = ();
    type Record = OtaTransaction;

    fn load(&mut self) -> Result<Option<Self::Record>, Self::Error> {
        Ok(self.record.clone())
    }

    fn commit(&mut self, record: Option<&Self::Record>) -> Result<(), Self::Error> {
        self.record = record.cloned();
        Ok(())
    }
}

async fn load_transaction(space: &OtaConfigSpace) -> Result<Option<OtaTransaction>, OtaMetadataError> {
    let metadata = load_metadata(space).await?;
    Ok(transaction_from_staged(&metadata.staged))
}

async fn commit_transaction(
    space: &OtaConfigSpace,
    record: Option<&OtaTransaction>,
) -> Result<(), OtaMetadataError> {
    let mut metadata = load_metadata(space).await?;
    metadata.set_staged(staged_from_transaction(record)?);
    save_metadata(space, &metadata).await
}

/// Digest of the currently-running, validated firmware.
pub async fn active_digest(space: &OtaConfigSpace) -> String {
    load_metadata(space)
        .await
        .map(|metadata| metadata.active_digest)
        .unwrap_or_default()
}

/// The deployment_id of the currently-running, validated firmware.
pub async fn active_deployment_id(space: &OtaConfigSpace) -> String {
    load_metadata(space)
        .await
        .map(|metadata| metadata.active_deployment_id)
        .unwrap_or_default()
}

/// The partition actually booted, which can differ from the latest EWBT
/// entry after a fallback from an image with an invalid app header.
pub async fn active_slot(flash: &SharedFlash) -> String {
    let mut guard = flash.lock().await;
    let mut scratch = table_buffer();
    otadata::booted_slot(guard.storage(), &mut scratch)
        .map(String::from)
        .unwrap_or_default()
}

async fn current_ota_image(flash: &SharedFlash) -> BackendOutcome {
    let mut guard = flash.lock().await;
    otadata::read_entries(guard.storage(), &mut table_buffer())
        .map(|entries| otadata::image_outcome(&entries))
        .unwrap_or(BackendOutcome::Other)
}

pub type BootEntry = otadata::BootEntry;

/// Raw bootloader state, independent of the agent's own status.
pub async fn boot_info(flash: &SharedFlash) -> BootEntry {
    const UNKNOWN: BootEntry = BootEntry { slot: "", seq: 0, state: "unknown" };
    let mut guard = flash.lock().await;
    otadata::read_entries(guard.storage(), &mut table_buffer())
        .ok()
        .and_then(|entries| otadata::boot_entry(&entries))
        .unwrap_or(UNKNOWN)
}

/// Verifies the already-programmed inactive slot and publishes it as a
/// normal FiBeWI staged transaction. No alternate OTA/write path exists:
/// factory flashing merely placed the bytes there ahead of time.
pub async fn stage_preloaded_agent(
    flash: &SharedFlash,
    ota_config: &OtaConfigSpace,
    image: PreloadedAgent,
) -> Result<&'static str, PreloadedAgentError> {
    let expected = parse_digest(image.digest).ok_or(PreloadedAgentError::BadDigest)?;

    let (target, computed) = {
        let mut guard = flash.lock().await;
        let target = write_target_locked(&mut guard).ok_or(PreloadedAgentError::NoTarget)?;
        if image.size as usize > target.size {
            return Err(PreloadedAgentError::TooLarge);
        }

        let mut hasher = Sha256::new();
        let mut buf = [0u8; 4096];
        let mut offset = 0u32;
        while offset < image.size {
            let remaining = (image.size - offset) as usize;
            let take = remaining.min(buf.len());
            ReadNorFlash::read(
                guard.storage(),
                target.offset + offset,
                &mut buf[..take],
            )
            .map_err(|_| PreloadedAgentError::Flash)?;
            hasher.update(&buf[..take]);
            offset += take as u32;
        }
        (target, fibewi::Digest(hasher.finalize().into()))
    };

    if computed != expected {
        return Err(PreloadedAgentError::DigestMismatch);
    }

    let record = OtaTransaction::staged(
        String::from(image.deployment_id),
        fibewi::ArtifactRecord {
            id: String::from("firmware"),
            size: u64::from(image.size),
            digest: expected,
            target: String::from(target.slot.as_str()),
        },
    );
    commit_transaction(ota_config, Some(&record))
        .await
        .map_err(PreloadedAgentError::Metadata)?;

    Ok(target.slot.as_str())
}

/// `POST /v1alpha1/ota/prepare` request body (contrat §4). `artifact` and
/// `idf_version` are accepted but not declared here -- this agent isn't
/// ESP-IDF, so there's no meaningful running version to compare `idf_version`
/// against; serde ignores fields a struct doesn't declare, same as every
/// other `POST` body in `agent.rs`.
#[derive(Deserialize)]
pub struct PrepareRequest {
    pub size: u32,
    pub chip: String,
    pub partition_layout: String,
}

/// `POST /v1alpha1/ota/prepare` response body (contrat §4).
#[derive(Serialize)]
pub struct PrepareResponse {
    accepted: bool,
    target_slot: Option<&'static str>,
    reason: Option<&'static str>,
}

fn refuse(reason: &'static str) -> PrepareResponse {
    PrepareResponse { accepted: false, target_slot: None, reason: Some(reason) }
}

/// Validates compat *before* a single byte transfers (contrat §3: "un
/// binaire esp32-s3 flashé sur esp32 ne boote pas").
pub async fn prepare(flash: &SharedFlash, ota_config: &OtaConfigSpace, req: &PrepareRequest) -> PrepareResponse {
    if req.chip != esp_metadata_generated::chip_pretty!() {
        return refuse("chip_mismatch");
    }
    if req.partition_layout != PARTITION_LAYOUT {
        return refuse("layout_mismatch");
    }

    // The guard must be released before `load_transaction` below: ConfigSpace
    // reads take this same (non-reentrant) `SharedFlash` mutex through the NVS
    // backend, so holding it across that call deadlocks the request.
    let target = {
        let mut flash_guard = flash.lock().await;
        write_target_locked(&mut flash_guard)
    };
    let Some(target) = target else {
        return refuse("busy");
    };
    if req.size as usize > target.size {
        return refuse("size_too_large");
    }
    // `Staged` will be superseded by `write_begin`, same as the PUT it
    // precedes -- accept. `Activating` is refused here too: reporting
    // `accepted` and then having the following PUT hit `write_begin`'s own
    // `Conflict` would be a prepare that lied.
    if let Ok(Some(record)) = load_transaction(ota_config).await {
        if !fibewi::embewi::can_supersede(Some(&record)) {
            return refuse("busy");
        }
    }
    PrepareResponse { accepted: true, target_slot: Some(target.slot.as_str()), reason: None }
}

/// Physical erase-block size used only for diagnostics and scratch allocation.
/// The actual erase/write mechanics and bounds checks live in `espbewi-ota`.
fn ota_erase_size() -> usize {
    <EspFlash as NorFlash>::ERASE_SIZE
}

/**
 * Native ESP flash block-erase size. OTA partitions are 64 KiB-aligned
 * (see partitions.csv), so erasing one of these ranges lets esp-storage use
 * the ROM block-erase command instead of sixteen 4 KiB sector erases.
 *
 * Durability stays sector-sized: only the erase is batched. Programming and
 * fibewi's durable watermark still advance every 4 KiB.
 */
fn ota_erase_batch_size() -> u64 {
    64 * 1024
}

/// In-RAM write session (see the module doc comment for why this doesn't
/// need to survive a reboot). One at a time, matching `firmware-c`'s own
/// single static session -- this device only ever serves one HTTP
/// connection at a time anyway.
struct WriteSession {
    /// Physical ESP partition selected once at begin. EWBT chooses the slot;
    /// `espbewi-ota` resolves that slot to this offset/size descriptor.
    partition: AppPartition,
    /// One erase block, heap-allocated so it never consumes an Embassy task
    /// stack frame. The external backend borrows and reuses it on every
    /// append/finish call; padding remains a physical-write detail only.
    scratch: Box<[u8]>,
    /// The generic engine: received/durable byte counts, the undurable
    /// tail, and the streaming digest -- see `fibewi::artifact`'s own
    /// doc comment. Everything sector-shaped stays out here, in
    /// [`EspArtifactStorage`]; the engine itself has no notion of it.
    engine: fibewi::WriteSession,
    /// When `write_begin` opened this session -- purely diagnostic, logged
    /// by `write_finish` (contrat §4's own `written`/digest reply carries
    /// no timing field).
    started_at: Instant,
    /// How many sectors have been erased+programmed so far -- purely
    /// diagnostic, alongside `started_at`. Derived from how far
    /// `engine.durable()` moves on each call (always a whole number of
    /// sectors, except `write_finish`'s own final partial one).
    sectors_flushed: u32,
    /// Logical prefix already erased in large flash blocks. This is advanced
    /// ahead of programming but never published as durable data: fibewi's
    /// own durable watermark remains sector-sized and only moves after the
    /// corresponding program operation succeeds.
    erased_through: u64,
    /// Number of native flash block erase operations requested, diagnostic
    /// only (the final range may be shorter only if partition geometry ever
    /// changes; the current A/B layout is block-aligned).
    erase_batches: u32,
    /// Frozen at the first PUT: what the image is (`deployment_id`,
    /// `digest`) and how big (`total`). Every later PUT of the session must
    /// repeat them exactly ([`write_params_match`]) and `write_finish`
    /// uses these, never whatever the last request happened to carry.
    params: SessionParams,
}

/// The identity of one write session, fixed by its first PUT.
pub struct SessionParams {
    pub deployment_id: String,
    /// `sha256:<64 hex>`, as sent.
    pub digest: String,
    /// Full image size: `Content-Range`'s total, or `Content-Length` for a
    /// monolithic PUT.
    pub total: u32,
}

static WRITE_SESSION: Mutex<CriticalSectionRawMutex, Option<WriteSession>> = Mutex::new(None);

pub async fn write_in_progress() -> bool {
    WRITE_SESSION.lock().await.is_some()
}

/// Bytes durably on flash -- see `fibewi::WriteSession::durable`'s own
/// doc comment. This is what the JSON `written` field reports to the
/// client: the point it's safe to resume *after a dropped connection* from.
pub async fn write_written() -> u32 {
    WRITE_SESSION.lock().await.as_ref().map_or(0, |s| s.engine.durable() as u32)
}

/// Bytes accepted into the session so far. Distinct from `write_written`
/// and used only for `write_plan`'s Continue-vs-Resync decision --
/// consecutive chunks of one *uninterrupted* PUT sequence declare their
/// `start` as "how much I've sent so far", which -- unless the connection
/// actually dropped -- is this, not `write_written` (which lags behind it
/// by up to one sector). Conflating the two would spuriously 416 a live
/// transfer whose chunk size doesn't happen to be a multiple of the flash
/// sector size.
pub async fn write_received() -> u32 {
    WRITE_SESSION.lock().await.as_ref().map_or(0, |s| s.engine.received() as u32)
}

/// `PUT /v1alpha1/ota/write`'s resume decision itself (contrat §4's
/// `Content-Range` protocol, decoupled from `Content-Range`'s own wire
/// format) is `fibewi::resume_plan`/`fibewi::is_complete` --
/// generic, `no_std`, host-tested in that crate. These two functions are
/// thin `u32`-to-`u64` adapters so callers keep writing `ota::Plan`/
/// `ota::write_plan`/`ota::write_is_final` unchanged; nothing about the
/// decision itself lives here anymore.
pub use fibewi::ResumePlan as Plan;

pub fn write_plan(has_range: bool, start: u32, in_progress: bool, written: u32) -> Plan {
    fibewi::resume_plan(has_range, u64::from(start), in_progress, u64::from(written))
}

pub fn write_is_final(has_range: bool, end: u32, total: u32) -> bool {
    fibewi::is_complete(has_range, u64::from(end), u64::from(total))
}

/// Whether a continuing PUT carries the same `deployment_id`, digest and
/// total as the session it claims to resume.
pub async fn write_params_match(params: &SessionParams) -> bool {
    WRITE_SESSION.lock().await.as_ref().is_some_and(|s| {
        s.params.deployment_id == params.deployment_id
            && s.params.digest.eq_ignore_ascii_case(&params.digest)
            && s.params.total == params.total
    })
}

pub enum BeginError {
    /// The next slot couldn't be resolved.
    Busy,
    /// The declared image doesn't fit the slot.
    TooLarge,
    /// A transaction is `Activating`: refused rather than superseded, since
    /// clearing it here could race the reboot into it (contrat: `409
    /// ota_busy`).
    Conflict,
    /// Superseding a `Staged` transaction (`commit(None)`) failed. The
    /// previous transaction may now be in an unknown state -- refusing to
    /// start a new write on top of that rather than risking two live at
    /// once.
    Storage(OtaMetadataError),
}

/// Starts (or restarts) a write session against whichever slot the
/// bootloader would currently hand out next. Always re-derived fresh here
/// rather than cached from `/ota/prepare`: `firmware-c`'s `write_begin`
/// does the same (see its own comment for why) -- prepare is a compat
/// pre-check, not a reservation.
///
/// Before this ever touches flash, whatever is currently staged is
/// resolved: a `Staged` transaction (written but never activated) is
/// explicitly superseded -- `commit(None)` clears NVS *before* the new
/// image's first byte is programmed, never after -- so a power cut at any
/// point during this write leaves either the old transaction (untouched,
/// still valid) or nothing staged (the new one incomplete and never
/// published), never NVS claiming an artifact that flash no longer holds
/// intact. An `Activating` transaction is refused outright: it is already
/// handed to the backend and racing a reboot into it. This is what makes
pub async fn write_begin(flash: &SharedFlash, ota_config: &OtaConfigSpace, params: SessionParams) -> Result<(), BeginError> {
    let target = {
        let mut flash_guard = flash.lock().await;
        write_target_locked(&mut flash_guard).ok_or(BeginError::Busy)?
    };
    if params.total as usize > target.size {
        return Err(BeginError::TooLarge);
    }
    // `params.digest` is already validated (`is_valid_digest`, in
    // `ota_write.rs`, before `write_begin` is ever reached): parsing it
    // here cannot actually fail. Handled as a real error rather than a
    // panic regardless -- an internal invariant slipping should refuse the
    // write, not crash the whole path.
    let Some(expected_digest) = parse_digest(&params.digest) else {
        warn!("ota: write_begin got an unparseable digest past validation, refusing");
        return Err(BeginError::Busy);
    };

    match load_transaction(ota_config).await.map_err(BeginError::Storage)? {
        None => {}
        Some(record) if fibewi::embewi::can_supersede(Some(&record)) => {
            commit_transaction(ota_config, None)
                .await
                .map_err(BeginError::Storage)?;
        }
        Some(_) => return Err(BeginError::Conflict),
    }

    *WRITE_SESSION.lock().await = Some(WriteSession {
        partition: target,
        scratch: alloc::vec![0u8; ota_erase_size()].into_boxed_slice(),
        engine: fibewi::WriteSession::begin(u64::from(params.total), expected_digest),
        started_at: Instant::now(),
        sectors_flushed: 0,
        erased_through: 0,
        erase_batches: 0,
        params,
    });
    Ok(())
}

/// Appends `data` to the session. `fibewi` owns streaming/durability;
/// `espbewi-ota::EspArtifactStorage` owns the physical erase/program work.
/// Handles `data` of any length, not just the HTTP handler's own
/// read-buffer size -- it may span several sectors in one call.
pub async fn write_chunk(flash: &SharedFlash, data: &[u8]) -> bool {
    let mut session_guard = WRITE_SESSION.lock().await;
    let Some(session) = session_guard.as_mut() else {
        return false;
    };

    // Never touch flash for a chunk the session would refuse anyway --
    // `engine.append` checks this too (`Error::TooLarge`), but checking it
    // here first avoids locking flash at all for a chunk that's already
    // doomed, same as the pre-`fibewi` code did.
    let received = session.engine.received();
    if u64::try_from(data.len()).ok().and_then(|len| received.checked_add(len)).is_none_or(|end| end > u64::from(session.params.total))
    {
        return false;
    }

    // Erase ahead in native 64 KiB flash blocks, but keep the actual write
    // and durable watermark sector-sized. This removes the dominant cost of
    // issuing one 4 KiB sector erase for every 4 KiB programmed while
    // preserving Content-Range resume precision and the power-cut invariant.
    let end_received = received + data.len() as u64;
    let erase_batch = ota_erase_batch_size();
    let desired_erased = end_received
        .div_ceil(erase_batch)
        .saturating_mul(erase_batch)
        .min(session.partition.size as u64);

    let mut flash_guard = flash.lock().await;
    let before = session.engine.durable();
    let raw_flash = flash_guard.storage();
    let ok = if desired_erased > session.erased_through {
        if erase_partition_range(
            raw_flash,
            session.partition,
            session.erased_through,
            desired_erased,
        )
        .is_err()
        {
            false
        } else {
            session.erase_batches +=
                ((desired_erased - session.erased_through) / erase_batch) as u32;
            session.erased_through = desired_erased;
            match EspArtifactStorage::new_pre_erased(raw_flash, session.partition, session.scratch.as_mut()) {
                Ok(mut backend) => session.engine.append(&mut backend, data).is_ok(),
                Err(_) => false,
            }
        }
    } else {
        match EspArtifactStorage::new_pre_erased(raw_flash, session.partition, session.scratch.as_mut()) {
            Ok(mut backend) => session.engine.append(&mut backend, data).is_ok(),
            Err(_) => false,
        }
    };
    let after = session.engine.durable();
    session.sectors_flushed += (after - before).div_ceil(ota_erase_size() as u64) as u32;
    ok
}

pub struct WriteFinishOk {
    pub written: u32,
    pub digest: String,
}

pub enum WriteFinishError {
    NotWriting,
    DigestMismatch,
    /// The session ended short of its declared total.
    Incomplete,
    /// The image was written and verified, but the staged record couldn't
    /// be persisted -- it must not be reported as `written`.
    Storage(OtaMetadataError),
}

/// Closes the write session: compares the digest computed *while writing*
/// (never a post-hoc flash re-read, per contrat §4) against the one the
/// session was opened with, and on a match persists the staged state
/// (contrat §6). Both the expected digest and the `deployment_id` come
/// from the session -- fixed by its first PUT, not by the last request.
pub async fn write_finish(flash: &SharedFlash, ota_config: &OtaConfigSpace) -> Result<WriteFinishOk, WriteFinishError> {
    let Some(session) = WRITE_SESSION.lock().await.take() else {
        return Err(WriteFinishError::NotWriting);
    };
    let WriteSession {
        partition,
        mut scratch,
        engine,
        started_at,
        mut sectors_flushed,
        erased_through,
        erase_batches,
        params,
    } = session;
    let slot = partition.slot;

    // Every accepted byte passed through write_chunk first, which erases the
    // containing native flash block before the engine can program it.
    if engine.received() > erased_through {
        warn!(
            "ota: internal erase watermark {} behind received {}",
            erased_through,
            engine.received()
        );
        return Err(WriteFinishError::Incomplete);
    }

    // The image's own size rarely lands on a sector boundary: `finish`
    // flushes whatever's left buffered (a final, partial sector) before
    // checking completeness and the digest. The external ESP backend owns
    // the final-block padding and erase/program geometry.
    let before = engine.durable();
    let committed = {
        let mut flash_guard = flash.lock().await;
        let raw_flash = flash_guard.storage();
        let mut backend = match EspArtifactStorage::new_pre_erased(raw_flash, partition, scratch.as_mut()) {
            Ok(backend) => backend,
            Err(_) => {
                warn!("ota: ESP artifact backend could not be constructed");
                return Err(WriteFinishError::Incomplete);
            }
        };
        match engine.finish(&mut backend) {
            Ok(committed) => committed,
            Err(fibewi::Error::DigestMismatch(computed)) => {
                warn!("ota: digest mismatch, attendu={} calculé={}", params.digest, format_digest(&computed));
                return Err(WriteFinishError::DigestMismatch);
            }
            Err(fibewi::Error::Incomplete { durable }) => {
                warn!("ota: session ended at {durable} of {} octets", params.total);
                return Err(WriteFinishError::Incomplete);
            }
            Err(e) => {
                warn!("ota: write finish failed ({e:?})");
                return Err(WriteFinishError::Incomplete);
            }
        }
    };
    sectors_flushed += (committed.size - before).div_ceil(ota_erase_size() as u64) as u32;

    let digest = format_digest(&committed.digest);
    let record = OtaTransaction::staged(
        params.deployment_id.clone(),
        fibewi::ArtifactRecord { id: String::from("firmware"), size: committed.size, digest: committed.digest, target: String::from(slot.as_str()) },
    );
    commit_transaction(ota_config, Some(&record))
        .await
        .map_err(WriteFinishError::Storage)?;
    let elapsed = started_at.elapsed();
    info!(
        "ota: write OK {} octets ({} secteurs programmés, {} blocs erase de {} KiB) en {}ms slot={} -> staged=written",
        committed.size,
        sectors_flushed,
        erase_batches,
        ota_erase_batch_size() / 1024,
        elapsed.as_millis(),
        slot.as_str()
    );
    Ok(WriteFinishOk { written: committed.size as u32, digest })
}

/// `POST /v1alpha1/ota/activate` (contrat §4): points the bootloader at the
/// staged slot and arms `OtaImageState::New` (which it promotes to
/// `PendingVerify` on the next boot). Reads the target slot from the persisted
/// ConfigSpace `staged` state, not the in-RAM write session -- matches `firmware-c`'s
/// own fallback ("Reprise après reboot de l'agent entre write et
/// activate"), and works identically whether or not this device rebooted
/// since `/ota/write` finished.
pub async fn activate(flash: &SharedFlash, ota_config: &OtaConfigSpace, deployment_id: &str) -> Result<&'static str, ActivateError> {
    // Record the intent first: if ConfigSpace persistence refuses it, nothing has changed yet
    // and the caller gets an error instead of a reboot into a slot whose
    // staged record disagrees with `otadata`. `fibewi::activate` checks
    // `Staged` + identity (against the *transaction's* id, i.e.
    // `deployment_id` -- never an artifact's own id) and durably commits
    // the transition to `Activating` before returning.
    let current = load_transaction(ota_config)
        .await
        .map_err(ActivateError::Storage)?;
    // Reject a stale or unknown platform target before persisting Activating.
    let slot = current.as_ref()
        .and_then(|record| record.artifacts.first())
        .and_then(|artifact| match artifact.target.as_str() {
            "ota_0" => Some(AppSlot::Ota0),
            "ota_1" => Some(AppSlot::Ota1),
            _ => None,
        })
        .ok_or(ActivateError::NotStaged)?;
    let mut meta = MemoryTransactionMetadata { record: current };
    let activating = fibewi::activate(&mut meta, &String::from(deployment_id)).map_err(|e| match e {
        fibewi::Error::NotStaged => ActivateError::NotStaged,
        fibewi::Error::IdentityMismatch => ActivateError::DeploymentMismatch,
        _ => ActivateError::NotStaged,
    })?;
    commit_transaction(ota_config, meta.record.as_ref())
        .await
        .map_err(ActivateError::Storage)?;
    // `fibewi::activate` already refused an empty artifact list.
    let ok = otadata_activate(flash, slot).await.is_ok();
    if !ok {
        // Best effort: back to `Staged` so a retry of `activate` is possible.
        let reverted = activating.with_state(TransactionState::Staged);
        if commit_transaction(ota_config, Some(&reverted)).await.is_err() {
            warn!("ota: activate failed and the staged record couldn't be restored to `written`");
        }
        return Err(ActivateError::NotStaged);
    }

    info!("ota: activate dep={deployment_id} -> slot={} prêt, reboot imminent", slot.as_str());
    Ok(slot.as_str())
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

/// Records the validated image's digest and `deployment_id` as the active
/// ones. Idempotent, so an interrupted validation can be finished at the
/// next boot ([`Action::FinishInterruptedActivation`]).
async fn promote_staged(ota_config: &OtaConfigSpace, staged: &Staged) -> Result<(), OtaMetadataError> {
    let mut metadata = load_metadata(ota_config).await?;
    metadata.promote_staged(staged);
    save_metadata(ota_config, &metadata).await
}

/// Promotes the staged record once the image is confirmed, then forgets it.
/// If a write fails the `activating` record is kept and the agent goes
/// `Degraded`: the image is valid and stays so (never rolled back over
/// bookkeeping), and the next boot -- bootloader `Valid`, same slot, still
/// `activating` -- completes this promotion.
async fn finish_validation(ota_config: &OtaConfigSpace, staged: &Staged) {
    if promote_staged(ota_config, staged).await.is_err() {
        warn!("ota: validated image's digest/deployment_id couldn't be persisted, will retry at next boot");
        agent::set_state(agent::State::Degraded);
        return;
    }
    if clear_staged(ota_config).await.is_err() {
        warn!("ota: staged record couldn't be cleared after validation, will retry at next boot");
        agent::set_state(agent::State::Degraded);
        return;
    }
    agent::set_state(agent::State::Running);
    info!("ota: validation done (deployment_id={})", staged.deployment_id);
}

/// Confirms the just-self-checked image with the bootloader and cancels its
/// pending rollback. Only ever called after every self-check passes
/// (contrat §3: "mark_valid n'est appelé QUE si tous les checks passent").
async fn mark_valid(flash: &'static SharedFlash, ota_config: &'static OtaConfigSpace) {
    let staged = staged(ota_config).await;
    if let Err(e) = otadata_confirm(flash).await {
        // Couldn't even record validation -- don't claim `running` over an
        // image `embewi-boot` doesn't agree is confirmed.
        warn!("ota: couldn't confirm the running image (code {}), rolling back", e as u8);
        mark_invalid_and_reboot(flash).await;
    }
    // `otadata_confirm` only returns `Ok` once the committed `Valid` entry
    // has been read back and decoded exactly as written (`espbewi_ota::otadata::confirm`).
    // Only now does the anti-freeze watchdog come off: a freeze anywhere
    // before this point -- including one the self-check race itself can't
    // catch -- still resets into a `Pending` entry embewi-boot rolls back.
    disable_boot_watchdog();
    finish_validation(ota_config, &staged).await;
}

/// The rollback path (contrat §3): marks the image invalid and resets.
/// Never returns -- on reboot, `embewi-boot` sees `Invalid`/`Aborted` (or a
/// stuck `Pending`, if even this much couldn't complete, itself turned
/// `Aborted` on the next boot) and falls back to the previous slot on its
/// own; this agent doesn't drive that part.
async fn mark_invalid_and_reboot(flash: &'static SharedFlash) -> ! {
    if let Err(e) = otadata_reject(flash).await {
        warn!("ota: couldn't record rejection (code {}), resetting anyway", e as u8);
    }
    warn!("ota: self-check failed, marking image invalid and rebooting for rollback");
    // The anti-freeze watchdog (armed for this whole pending_verify window,
    // see `arm_boot_watchdog`) is deliberately left running here, not
    // disabled: this reset is about to happen anyway, and if *this* call
    // itself somehow never returns, the watchdog is still the backstop.
    //
    // Gives the log line above time to actually reach the WebSocket log
    // stream/serial console before the reset cuts it off.
    Timer::after(Duration::from_millis(200)).await;
    esp_hal::system::software_reset();
}

// --- anti-freeze watchdog ----------------------------------------------
//
// `esp_hal::init()` unconditionally disables every watchdog on the chip
// (there's no `Config` option to keep one running) as part of its normal
// hardware bring-up -- so a watchdog `embewi-boot` armed before jumping here
// does not survive into this agent; only the agent's own code can protect
// its post-`init()` startup. `arm_boot_watchdog` is called right after
// `TimerGroup::new(peripherals.TIMG0)`/`esp_rtos::start` (`src/bin/main.rs`)
// -- not any earlier: `TimerGroup::new`'s first use of TIMG0 resets the
// whole peripheral block (`PeripheralClockControl`'s refcount going 0 -> 1),
// which would silently wipe out a watchdog armed before that call. From
// there it covers everything through `on_boot`'s decision -- physical flash initialization,
// not just the bounded self-check race inside `on_boot` itself. `on_boot`
// disables it again within milliseconds for every outcome except
// a genuine `pending_verify` self-check, where [`feed_boot_watchdog`] keeps
// it running until the image is durably confirmed
// ([`mark_valid`]/[`disable_boot_watchdog`]).
//
// espbewi-watchdog owns the TIMG0 register access. The application chooses
// the deadline and the points at which its boot protection ends.

/// Arms the anti-freeze watchdog. Call exactly once, right after
/// `TimerGroup::new(peripherals.TIMG0)` (see the module section comment
/// above for why not any earlier).
pub fn arm_boot_watchdog() {
    espbewi_watchdog::arm_ms(WATCHDOG_DEADLINE_MS);
}

fn feed_boot_watchdog() {
    espbewi_watchdog::feed();
}

fn disable_boot_watchdog() {
    espbewi_watchdog::disable();
}

/// Runs the existing bounded FiBeWI/ESP confirmation gate after the caller
/// has decided that its own application prerequisites are satisfied.
pub async fn confirm_pending(
    flash: &'static SharedFlash,
    nvs_backend: &'static NvsConfigBackend,
    ota_config: &'static OtaConfigSpace,
) {
    // TEST/DEBUG ONLY (`fault-injection-freeze` feature, never in a
    // production image): starves the executor before the self-check's own
    // software deadline (below) can ever be polled -- the one failure mode
    // that deadline structurally can't catch, since it depends on the same
    // stuck executor. Only the hardware watchdog (`arm_boot_watchdog`,
    // already armed and fed by `on_boot` before this task was spawned) can
    // recover from this; if it doesn't, this loop runs forever.
    if cfg!(feature = "fault-injection-freeze") {
        warn!("ota: [fault-injection-freeze] spinning forever, only the hardware watchdog can save this boot");
        loop {
            core::hint::spin_loop();
        }
    }
    // contrat §3: bounded by a deadline, not just "run the checks" -- a
    // hung check must never leave the device stuck in `pending_verify`
    // forever. Uses a plain `embassy_time::Timer` race rather than a
    // hardware watchdog: `LPWR` (the RTC peripheral the watchdog lives on)
    // is already owned by `http::run`'s own reboot mechanism for the
    // config page/`/reboot`/`/ota/activate` (see that module's doc comment
    // for why it needs RTC specifically, not `system::software_reset()`);
    // a plain software reset here is a faithful port of what `firmware-c`
    // itself does in this exact spot (an `esp_timer` deadline calling
    // `esp_restart()`, not a TWDT trip).
    match select(nvs_backend.self_check(), Timer::after(SELFCHECK_DEADLINE)).await {
        Either::First(true) => {
            // TEST/DEBUG ONLY (`fault-injection` feature, never in a
            // production image): reset right here, after the self-check
            // passed but before `confirm` ever runs -- so `otadata` is left
            // exactly as a real crash mid-`pending_verify` would leave it
            // (still `Pending`), for embewi-boot's rollback to act on on the
            // next boot. Deterministic, unlike timing a physical power-cut
            // against a window that normally closes before Wi-Fi even
            // reconnects.
            if cfg!(feature = "fault-injection") {
                warn!("ota: [fault-injection] self-check passed, resetting BEFORE confirm to exercise rollback");
                Timer::after(Duration::from_millis(200)).await;
                esp_hal::system::software_reset();
            }
            mark_valid(flash, ota_config).await
        }
        Either::First(false) => mark_invalid_and_reboot(flash).await,
        Either::Second(()) => {
            warn!("ota: self-check deadline exceeded, forcing a reset (bootloader will roll back)");
            esp_hal::system::software_reset();
        }
    }
}

/// Called once at boot (`src/bin/main.rs`): reconciles the persisted staged
/// record with what the bootloader actually booted (contrat §3's "cœur dur
/// du projet"). The decision itself is [`fibewi::reconcile`], a pure
/// table host-tested in that crate; this only gathers its inputs and
/// applies the outcome. This is the only place `agent::State` is driven
/// from `Booting`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootDisposition {
    Stable,
    PendingVerify,
}

pub async fn on_boot(
    flash: &'static SharedFlash,
    nvs_backend: &'static NvsConfigBackend,
    ota_config: &'static OtaConfigSpace,
) -> BootDisposition {
    let staged = staged(ota_config).await;
    let image = current_ota_image(flash).await;
    let booted = active_slot(flash).await;
    let booted_is_staged = (!booted.is_empty()).then(|| booted == staged.slot);
    let staged_state = match staged.stage {
        Stage::None => None,
        Stage::Written => Some(TransactionState::Staged),
        Stage::Activating => Some(TransactionState::Activating),
    };
    let action = fibewi::reconcile(staged_state, image, booted_is_staged);
    info!("ota: boot slot={booted:?} staged={} image={image:?} -> {action:?}", staged.stage.as_str());

    match action {
        Action::AwaitConfirmation => {
            agent::set_state(agent::State::PendingVerify);
            warn!("ota: image is PENDING_VERIFY, application confirmation required");
            // The application decides when it is safe to call confirm_pending.
            // FiBeWI knows nothing about those application prerequisites.
            feed_boot_watchdog();
            return BootDisposition::PendingVerify;
        }
        Action::RollbackUnaccounted => {
            warn!("ota: PENDING_VERIFY image not accounted for by the staged record, rolling back");
            agent::set_state(agent::State::Rollback);
            mark_invalid_and_reboot(flash).await;
        }
        Action::Nothing | Action::KeepStaged => {}
        Action::ClearStale => {
            warn!("ota: stale staged record ({}), clearing", staged.stage.as_str());
            if clear_staged(ota_config).await.is_err() {
                warn!("ota: stale staged record couldn't be cleared");
            }
        }
        Action::FinishInterruptedActivation => {
            warn!("ota: finishing a validation interrupted before its bookkeeping");
            // Runs the same path as a live validation; `Degraded` (set by
            // it on failure) must not be overwritten below.
            finish_validation(ota_config, &staged).await;
            if agent::state() == agent::State::Degraded {
                disable_boot_watchdog();
                return BootDisposition::Stable;
            }
        }
        // `Action` is `#[non_exhaustive]`: fibewi is not at a stable API
        // yet, and a future variant must not silently fall into one of the
        // arms above. Nothing destructive on an outcome this build doesn't
        // recognize -- same policy as `running_matches_staged: None`.
        _ => {
            warn!("ota: reconcile returned an action this build doesn't recognize, doing nothing");
        }
    }

    // Not a pending_verify boot after all (or one that needed no further
    // action): the anti-freeze window `arm_boot_watchdog` opened at the top
    // of `main` is over.
    disable_boot_watchdog();

    // The NVS canary round-trip `/health` reports on (during
    // `pending_verify` the self-check task runs it instead).
    if !nvs_backend.self_check().await {
        warn!("ota: boot NVS self-check failed, /health will report storage=fail");
    }
    agent::set_state(agent::State::Running);
    BootDisposition::Stable
}

/// Explicit application-triggered rejection of the current candidate.
pub async fn reject_pending(flash: &'static SharedFlash) -> ! {
    mark_invalid_and_reboot(flash).await
}
