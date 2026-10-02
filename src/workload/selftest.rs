//! TEST/DEBUG ONLY (`workload-selftest`): hardware gate of the Workload storage.
//!
//! The phase is derived from what is observable in flash, so it survives resets:
//!
//! 1. `NoWorkload`: write 1 MiB to slot A, activate (no-op supervisor), confirm
//!    -> `Valid(A)`; then write 4 MiB to slot B and **reset before staging it**
//!    (power cut between "artifact complete" and "metadata staged commit").
//! 2. `Valid(A)` and slot B already holds the 4 MiB artifact (digest read back):
//!    the cut was harmless; stage B (recovery re-verifies it by read-back), reset.
//! 3. `Staged{A,B}`: verify both artifacts by SHA-256 read-back -> PASS.
//!
//! While slot A is written, a second task of this module hammers ConfigSpace and
//! OTM2 reads to prove the single flash lock neither deadlocks nor corrupts.
//! Never loads or executes anything.

use core::cell::Cell;

use embassy_futures::join::join;
use embassy_time::{Duration, Instant, Timer};
use iobewi_esp_workload::engine::machine::{Prepared, Recovery, WorkloadActivator};
use iobewi_esp_workload::model::{ArtifactDescriptor, RuntimeApi, Side, UpdateRequest, WorkloadSupervisor};
use iobewi_esp_workload::EspWorkloadStorage;
use crate::workload::WorkloadService;
use iobewi_esp_config_space::NvsConfigBackend;
use iobewi_ota::{Committed, Digest};
use sha2::{Digest as _, Sha256};

const P1_SEED: u32 = 0x1111_1111;
const P1_SIZE: u32 = 1024 * 1024;
const P2_SEED: u32 = 0x2222_2222;
const P2_SIZE: u32 = 4 * 1024 * 1024;
const CHUNK: usize = 16 * 1024;

struct Noop;
impl WorkloadSupervisor for Noop {
    fn switch_to(&mut self, _side: Side) {}
    fn restore(&mut self, _side: Side) {}
}
impl WorkloadActivator for Noop {
    fn stop(&mut self) {}
}

/// Deterministic pseudo-random bytes (xorshift32), so the expected digest can be
/// computed without storing the artifact.
struct Pattern {
    state: u32,
}

impl Pattern {
    fn new(seed: u32) -> Self {
        Self { state: seed }
    }

    fn fill(&mut self, out: &mut [u8]) {
        for byte in out {
            self.state ^= self.state << 13;
            self.state ^= self.state >> 17;
            self.state ^= self.state << 5;
            *byte = (self.state >> 11) as u8;
        }
    }
}

fn expected_digest(seed: u32, size: u32) -> [u8; 32] {
    let mut pattern = Pattern::new(seed);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 4096];
    let mut left = size as usize;
    while left > 0 {
        let take = left.min(buf.len());
        pattern.fill(&mut buf[..take]);
        hasher.update(&buf[..take]);
        left -= take;
    }
    hasher.finalize().into()
}

fn request(version: &str, seed: u32, size: u32) -> UpdateRequest {
    UpdateRequest::workload(
        ArtifactDescriptor {
            id: "selftest".into(),
            version: version.into(),
            digest: expected_digest(seed, size),
            size: u64::from(size),
        },
        RuntimeApi::new(1, 0),
    )
}

/// Streams the pattern into the prepared slot; returns the verified result and the elapsed ms.
async fn write_pattern(
    storage: &EspWorkloadStorage,
    prepared: &Prepared,
    seed: u32,
    size: u32,
) -> Option<(Committed, u64)> {
    let started = Instant::now();
    let mut writer = storage.writer(prepared);
    let mut pattern = Pattern::new(seed);
    let mut buf = alloc::vec![0u8; CHUNK];
    let mut left = size as usize;
    while left > 0 {
        let take = left.min(CHUNK);
        pattern.fill(&mut buf[..take]);
        if !writer.append(storage.access(), &buf[..take]).await {
            log::error!("selftest: slot write failed at {} B", size as usize - left);
            return None;
        }
        left -= take;
        Timer::after(Duration::from_millis(0)).await; // yield between chunks
    }
    match writer.finish(storage.access()).await {
        Ok(committed) => Some((committed, started.elapsed().as_millis())),
        Err(e) => {
            log::error!("selftest: slot finish failed: {e:?}");
            None
        }
    }
}

/// ConfigSpace + OTM2 reads while the flash is busy; reports progress and the
/// worst single-operation latency (a hang would show as zero progress).
async fn load(
    storage: &EspWorkloadStorage,
    config: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
    stop: &Cell<bool>,
) -> (u32, u32, u64) {
    let (mut config_ok, mut meta_ok, mut worst_ms) = (0u32, 0u32, 0u64);
    while !stop.get() {
        let t = Instant::now();
        if config.load().await.is_ok() {
            config_ok += 1;
        }
        if storage.recover().await.is_ok() {
            meta_ok += 1;
        }
        worst_ms = worst_ms.max(t.elapsed().as_millis());
        Timer::after(Duration::from_millis(20)).await;
    }
    (config_ok, meta_ok, worst_ms)
}

async fn reset(reason: &str) -> ! {
    log::warn!("selftest: resetting ({reason})");
    Timer::after(Duration::from_millis(400)).await;
    iobewi_esp_reset::software_reset()
}

#[embassy_executor::task]
pub async fn run(
    service: &'static WorkloadService,
    config: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
) {
    // Let Wi-Fi/services settle: the test competes with them for the flash on purpose.
    Timer::after(Duration::from_secs(8)).await;
    let Ok(storage) = service.storage() else { return };
    let state = match storage.recover().await {
        Ok(state) => state,
        Err(e) => {
            log::error!("selftest: FAIL recover: {e:?}");
            return;
        }
    };
    log::info!("selftest: start, otm2 recovery = {state:?}");

    match state {
        Recovery::NoWorkload => phase1(storage, config).await,
        Recovery::Valid(Side::A) => phase2(storage).await,
        Recovery::Staged { active: Some(Side::A), candidate: Side::B } => phase3(storage).await,
        other => log::error!("selftest: FAIL unexpected state {other:?}"),
    }
}

async fn phase1(
    storage: &EspWorkloadStorage,
    config: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
) {
    // Perf: erase a whole slot (maintenance path), for an order of magnitude.
    let t = Instant::now();
    if storage.erase_slot(Side::B).await.is_err() {
        log::error!("selftest: FAIL erase slot B");
        return;
    }
    log::info!("selftest: perf erase whole slot ({} B): {} ms", storage.layout().max_artifact_size(), t.elapsed().as_millis());

    // Write slot A (1 MiB) while ConfigSpace/OTM2 reads compete for the flash lock.
    let prepared = match storage.prepare(&request("selftest-1", P1_SEED, P1_SIZE)).await {
        Ok(p) => p,
        Err(e) => {
            log::error!("selftest: FAIL prepare A: {e:?}");
            return;
        }
    };
    let stop = Cell::new(false);
    let (written, (config_ok, meta_ok, worst_ms)) = join(
        async {
            let result = write_pattern(storage, &prepared, P1_SEED, P1_SIZE).await;
            stop.set(true);
            result
        },
        load(storage, config, &stop),
    )
    .await;
    let Some((committed, ms)) = written else { return };
    log::info!("selftest: perf write 1 MiB to slot A: {ms} ms; concurrent config reads {config_ok}, otm2 reads {meta_ok}, worst op {worst_ms} ms");
    if config_ok == 0 || meta_ok == 0 {
        log::error!("selftest: FAIL no progress under flash contention (possible starvation)");
        return;
    }
    if let Err(e) = storage.commit_staged(&prepared, &committed).await {
        log::error!("selftest: FAIL commit_staged A: {e:?}");
        return;
    }
    if storage.activate(&mut Noop, RuntimeApi::new(1, 0)).await.is_err() || storage.confirm().await.is_err() {
        log::error!("selftest: FAIL activate/confirm A");
        return;
    }
    log::info!("selftest: slot A written, staged, activated (stub supervisor), confirmed -> Valid(A)");
    phase2(storage).await
}

async fn phase2(storage: &EspWorkloadStorage) {
    let want = expected_digest(P2_SEED, P2_SIZE);
    let prepared = match storage.prepare(&request("selftest-2", P2_SEED, P2_SIZE)).await {
        Ok(p) => p,
        Err(e) => {
            log::error!("selftest: FAIL prepare B: {e:?}");
            return;
        }
    };
    // Is slot B already holding the complete 4 MiB artifact (the cut happened last boot)?
    if matches!(storage.read_digest(prepared.slot, P2_SIZE).await, Ok(d) if d == want) {
        log::info!("selftest: powerloss recovery OK: slot B holds a complete artifact but the selection is still Valid(A) and nothing is staged");
        // The artifact was verified by an independent SHA-256 read-back, so it may be staged now.
        let committed = Committed { size: u64::from(P2_SIZE), digest: Digest(want) };
        if let Err(e) = storage.commit_staged(&prepared, &committed).await {
            log::error!("selftest: FAIL commit_staged B: {e:?}");
            return;
        }
        log::info!("selftest: slot B staged (candidate), resetting to prove recovery of the Staged state");
        reset("verify Staged recovery").await
    }
    // First time: write the 4 MiB artifact, then lose power before staging it.
    log::info!("selftest: writing 4 MiB to slot B, then cutting power before the staged commit");
    let Some((_committed, ms)) = write_pattern(storage, &prepared, P2_SEED, P2_SIZE).await else { return };
    log::info!("selftest: perf write 4 MiB to slot B: {ms} ms (erase-ahead included)");
    reset("power cut between artifact complete and staged commit").await
}

async fn phase3(storage: &EspWorkloadStorage) {
    let Ok(Some(record)) = storage.record().await else {
        log::error!("selftest: FAIL no OTM2 record");
        return;
    };
    let a = record.meta[0];
    let b = record.meta[1];
    let a_ok = matches!(storage.read_digest(Side::A, a.size).await, Ok(d) if d == expected_digest(P1_SEED, P1_SIZE) && d == a.digest);
    let b_ok = matches!(storage.read_digest(Side::B, b.size).await, Ok(d) if d == expected_digest(P2_SEED, P2_SIZE) && d == b.digest);
    log::info!(
        "selftest: after reset: state {:?} active {:?} candidate {:?}; slot A digest {} slot B digest {}; requires {}.{}",
        record.state, record.active, record.candidate,
        if a_ok { "OK" } else { "BAD" }, if b_ok { "OK" } else { "BAD" },
        b.requires.major, b.requires.minor,
    );
    if a_ok && b_ok {
        log::info!("selftest: PASS (write A, activate+confirm A, write B, powerloss before staging ignored, B staged, state recovered after reset, digests verified)");
    } else {
        log::error!("selftest: FAIL digest mismatch after recovery");
    }
}
