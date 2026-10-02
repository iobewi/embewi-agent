//! S18 **validation** runtime backend (feature `workload-supervisor-probe`): the minimal
//! thing that really executes and can be observed, so the Supervisor's state machine can
//! be validated on hardware. It is **not** the Workload runtime and its artifact header
//! (`S18PROBE`, see `iobewi_workload_ota::probe`) is not an ABI.
//!
//! The probe is one embassy task that increments a counter every period. Health is
//! derived from real progress: `Healthy` while the task is alive and the counter advanced
//! recently, `Unhealthy` when it froze (or a fault says so), `Unknown` when nothing runs.
//! `stop` signals the task and waits until it has really exited (no zombie task);
//! `start` refuses to overlap a running probe. Faults come from the artifact header, so a
//! test can stage an artifact that fails to start, freezes, reports unhealthy, or resets the
//! device inside `start`/`stop` (to cut power during `Activating` / `RollingBack`).

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Duration, Instant, Timer};
use iobewi_esp_workload::engine::probe::{HEADER_LEN, ProbeFault, ProbeHeader};
use iobewi_esp_workload::engine::supervisor::{ArtifactReader, Health, Identity, RuntimeError, WorkloadRuntime};

static CURRENT: Mutex<CriticalSectionRawMutex, RefCell<Option<(Identity, ProbeFault)>>> =
    Mutex::new(RefCell::new(None));
static ALIVE: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
static COUNTER: AtomicU32 = AtomicU32::new(0);
static LAST_PROGRESS_MS: AtomicU32 = AtomicU32::new(0);
static PERIOD_MS: AtomicU32 = AtomicU32::new(500);

fn now_ms() -> u32 {
    Instant::now().as_millis() as u32
}

#[embassy_executor::task]
async fn probe_task(period_ms: u32, fault: ProbeFault) {
    ALIVE.store(true, Ordering::SeqCst);
    LAST_PROGRESS_MS.store(now_ms(), Ordering::SeqCst);
    let mut ticks = 0u32;
    while !STOP.load(Ordering::SeqCst) {
        Timer::after(Duration::from_millis(u64::from(period_ms))).await;
        ticks += 1;
        let frozen = fault == ProbeFault::FreezeAfterStart && ticks > 4;
        if !frozen {
            COUNTER.store(COUNTER.load(Ordering::SeqCst).wrapping_add(1), Ordering::SeqCst);
            LAST_PROGRESS_MS.store(now_ms(), Ordering::SeqCst);
        }
        if ticks % 20 == 0 {
            log::info!("workload probe: counter={} frozen={frozen}", COUNTER.load(Ordering::SeqCst));
        }
    }
    ALIVE.store(false, Ordering::SeqCst);
}

pub struct ProbeRuntime {
    spawner: Spawner,
}

impl ProbeRuntime {
    pub fn new(spawner: Spawner) -> Self {
        Self { spawner }
    }

    /// Stop the task and wait until it really exited. Idempotent.
    async fn stop_inner(&self) {
        let current = CURRENT.lock(|c| c.borrow_mut().take());
        if ALIVE.load(Ordering::SeqCst) {
            STOP.store(true, Ordering::SeqCst);
            for _ in 0..100 {
                if !ALIVE.load(Ordering::SeqCst) {
                    break;
                }
                Timer::after(Duration::from_millis(20)).await;
            }
            if ALIVE.load(Ordering::SeqCst) {
                log::error!("workload probe: task did not stop in time");
            }
            if let Some((identity, ProbeFault::ResetOnStop)) = &current {
                log::warn!("workload probe: fault ResetOnStop in stop() of {} -> resetting", identity.version);
                Timer::after(Duration::from_millis(300)).await;
                iobewi_esp_reset::software_reset();
            }
        }
    }
}

impl WorkloadRuntime for ProbeRuntime {
    async fn start<R: ArtifactReader>(&self, artifact: &Identity, reader: &R) -> Result<(), RuntimeError> {
        self.stop_inner().await;
        let mut head = [0u8; HEADER_LEN];
        reader.read(0, &mut head).await?;
        let header = ProbeHeader::parse(&head).map_err(|_| RuntimeError { reason: "not a probe artifact" })?;
        log::info!(
            "workload probe: start id={} version={} size={} fault={:?}",
            artifact.id, artifact.version, artifact.size, header.fault
        );
        match header.fault {
            ProbeFault::FailStart => return Err(RuntimeError { reason: "fault: start fails" }),
            ProbeFault::ResetOnStart => {
                log::warn!("workload probe: fault ResetOnStart -> resetting inside start()");
                Timer::after(Duration::from_millis(300)).await;
                iobewi_esp_reset::software_reset();
            }
            _ => {}
        }
        COUNTER.store(0, Ordering::SeqCst);
        STOP.store(false, Ordering::SeqCst);
        PERIOD_MS.store(u32::from(header.period_ms), Ordering::SeqCst);
        CURRENT.lock(|c| *c.borrow_mut() = Some((artifact.clone(), header.fault)));
        let token = probe_task(u32::from(header.period_ms), header.fault).map_err(|_| RuntimeError { reason: "task slot busy" })?;
        self.spawner.spawn(token);
        for _ in 0..100 {
            if ALIVE.load(Ordering::SeqCst) {
                return Ok(());
            }
            Timer::after(Duration::from_millis(10)).await;
        }
        CURRENT.lock(|c| *c.borrow_mut() = None);
        Err(RuntimeError { reason: "probe task did not start" })
    }

    async fn stop(&self) {
        self.stop_inner().await;
    }

    async fn health(&self) -> Health {
        let Some((_, fault)) = CURRENT.lock(|c| c.borrow().clone()) else { return Health::Unknown };
        if !ALIVE.load(Ordering::SeqCst) {
            return Health::Unknown;
        }
        if fault == ProbeFault::HealthFail {
            return Health::Unhealthy;
        }
        let window = (PERIOD_MS.load(Ordering::SeqCst) * 3).max(2000);
        if now_ms().wrapping_sub(LAST_PROGRESS_MS.load(Ordering::SeqCst)) <= window {
            Health::Healthy
        } else {
            Health::Unhealthy
        }
    }

    async fn running(&self) -> Option<Identity> {
        if !ALIVE.load(Ordering::SeqCst) {
            return None;
        }
        CURRENT.lock(|c| c.borrow().as_ref().map(|(identity, _)| identity.clone()))
    }
}
