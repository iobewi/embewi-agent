//! Glue for the native Workload runtime (feature `workload-native`).
//!
//! The portable policy is `iobewi_workload_native::NativeRuntime`; the ESP32-S3 mechanics
//! (fixed RAM region, second core, log ring, clock) are `iobewi_esp_workload::native`. This
//! module only owns what belongs to the Agent: the reserved RAM and the two small tasks that
//! keep the Workload observable (health sampling, log forwarding).

use core::mem::MaybeUninit;

use embassy_time::{Duration, Timer};
use iobewi_esp_workload::model::RuntimeApi;
use iobewi_esp_workload::engine::supervisor::{Health, WorkloadRuntime};
use iobewi_esp_workload::native::boot_guard::{self, BootDecision, GuardRecord, ProvenHealth};
use iobewi_esp_workload::native::{EspNativeBackend, NativeRuntime, drain_logs, take_dropped_logs};

pub type Runtime = NativeRuntime<EspNativeBackend>;

/// The whole reclaimed `dram2` area (64 KiB), owned here so its placement is deterministic:
/// the first half is the Agent's heap, the second half is the Workload region (fixed address
/// `0x3FCE3700`, see `iobewi-workload-image`).
const RECLAIMED_TOTAL: usize = 64 * 1024;
const AGENT_HEAP_PART: usize = 32 * 1024;

#[esp_hal::ram(reclaimed)]
static mut RECLAIMED: MaybeUninit<[u8; RECLAIMED_TOTAL]> = MaybeUninit::uninit();

/// Gives the first half to the allocator and checks that the second half sits exactly where
/// the Workload images are linked. Returns `false` (Workloads then refuse to load) otherwise.
/// Call once, before any allocation, in place of the reclaimed `heap_allocator!`.
pub fn init_reclaimed_ram() -> bool {
    // SAFETY: called once at boot; the static is never used elsewhere.
    let base = (&raw mut RECLAIMED).cast::<u8>();
    unsafe {
        esp_alloc::HEAP.add_region(esp_alloc::HeapRegion::new(
            base,
            AGENT_HEAP_PART,
            esp_alloc::MemoryCapability::Internal.into(),
        ));
    }
    let region = base as usize + AGENT_HEAP_PART;
    let expected = iobewi_esp_workload::native::REGION_DBUS as usize;
    if region != expected {
        log::error!("workload: region at {region:#x}, images are linked for {expected:#x}: native Workloads disabled");
        return false;
    }
    log::info!("workload: native region {region:#x}..{:#x} reserved ({} B)", region + AGENT_HEAP_PART, AGENT_HEAP_PART);
    true
}

/// Crash-loop guard state. RTC fast RAM survives every reset except a power cycle (which
/// zeroes it, i.e. forgets); four words sealed by a checksum, because a reset can interrupt a
/// write. See `iobewi_workload_native::boot_guard`. Nothing about it is persisted in OTM2.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static mut GUARD_WORDS: [u32; 4] = [0; 4];

fn with_guard<R>(f: impl FnOnce(&mut GuardRecord) -> R) -> R {
    // SAFETY: only the boot code, the reboot task and the support task touch it, all on the
    // single Agent core and never concurrently with each other in a way that matters (each
    // access is a short read-modify-write of four words).
    let words = unsafe { (&raw mut GUARD_WORDS).read_volatile() };
    let mut record = GuardRecord::from_words(words);
    let out = f(&mut record);
    unsafe { (&raw mut GUARD_WORDS).write_volatile(record.to_words()) };
    out
}

/// Whether the Workload may be auto-started at this boot (and counts this attempt).
pub fn boot_decision() -> BootDecision {
    with_guard(boot_guard::on_boot)
}

/// Call right before a deliberate reset (operator reboot, Agent OTA): the next boot is not a crash.
pub fn mark_clean_reboot() {
    with_guard(|r| boot_guard::mark_clean_reboot(r));
}

pub fn new(cpu: esp_hal::system::CpuControl<'static>, provides: RuntimeApi, region_ok: bool) -> Runtime {
    NativeRuntime::new(EspNativeBackend::new(cpu, provides, region_ok))
}

/// Keeps health accurate (progress sampling) and forwards the Workload's log lines to the
/// Agent's logger, which already streams to the Core when connected.
#[embassy_executor::task]
pub async fn support_task(supervisor: &'static super::Supervisor) {
    // The logger writes synchronously (USB-Serial-JTAG): forwarding must be rate-limited or a
    // chatty Workload would monopolise the Agent's executor. 2 lines per 100 ms (20/s); the
    // rest stays in the ring, which drops when full, and the drops are reported at most once
    // a second.
    const LINES_PER_TICK: usize = 2;
    let mut forced_seen = 0u32;
    let mut quarantined_seen = 0u32;
    let mut dropped_total = 0u32;
    let mut ticks = 0u32;
    let mut proven = ProvenHealth::new();
    loop {
        let runtime = supervisor.runtime();
        runtime.sample();
        let forced = runtime.forced_stops();
        if forced != forced_seen {
            forced_seen = forced;
            log::warn!("workload: StopTimeout, the Workload ignored the stop request and was halted ({forced} so far)");
        }
        let quarantined = runtime.quarantines();
        if quarantined != quarantined_seen {
            quarantined_seen = quarantined;
            log::error!("workload: Workload died or wedged: its core was halted (quarantined); the Agent is unaffected");
        }
        drain_logs(LINES_PER_TICK, |level, text| {
            let text = core::str::from_utf8(text).unwrap_or("<non-utf8>");
            match level {
                1 => log::error!("workload: {text}"),
                2 => log::warn!("workload: {text}"),
                4 => log::debug!("workload: {text}"),
                _ => log::info!("workload: {text}"),
            }
        });
        dropped_total = dropped_total.saturating_add(take_dropped_logs());
        ticks += 1;
        if ticks % 10 == 0 {
            if dropped_total > 0 {
                log::warn!("workload: {dropped_total} log lines dropped (rate-limited)");
                dropped_total = 0;
            }
            // Healthy for long enough: forget earlier crashes (the guard's counter).
            let healthy = runtime.health().await == Health::Healthy;
            if proven.observe(healthy, embassy_time::Instant::now().as_millis()) {
                with_guard(|r| boot_guard::mark_proven_healthy(r));
                log::info!("workload: healthy for {} s, boot-failure counter cleared", boot_guard::PROVEN_HEALTHY_MS / 1000);
            }
        }
        Timer::after(Duration::from_millis(100)).await;
    }
}
