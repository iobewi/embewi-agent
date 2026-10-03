//! Workload OTA and Supervisor wiring (S18).
//!
//! * The capability comes from the on-flash partition table (partitions `wl_meta`,
//!   `workload_a`, `workload_b`, found by name); an older table is `Unsupported`.
//! * The persistent state is OTM2 (owned by the Workload OTA engine). The Supervisor
//!   reconciles it with the execution state through a runtime backend.
//! * Without the `test-probe-runtime` feature the Agent has **no** supervisor:
//!   activation answers `501 supervisor_unavailable` and nothing runs. With it, a
//!   minimal validation probe (see `probe`) stands in for the future runtime.

use iobewi_config_space::ConfigBackend;
use iobewi_esp_flash::SharedFlash;
use iobewi_esp_workload::engine::service::WorkloadOtaService;
use iobewi_esp_workload::model::RuntimeApi;
use iobewi_esp_workload::{Capability, EspFlashAccess, availability, probe};
use iobewi_workload_ota_http::Authorize;
use static_cell::StaticCell;

/// The runtime API this Agent provides to Workloads. **Single source of truth**:
/// the Workload OTA service, `/workload/ota/status`, the activation gate and the
/// Agent-OTA guard all read it from here. It is a compatibility contract between
/// Agent and Workload, not a kernel ABI; see `docs/dual-ota.md` in iobewi.
#[cfg(not(feature = "test-runtime-api-0-9"))]
pub const RUNTIME_API: RuntimeApi = RuntimeApi::new(1, 0);
/// TEST ONLY: an API no 1.x Workload can run on.
#[cfg(feature = "test-runtime-api-0-9")]
pub const RUNTIME_API: RuntimeApi = RuntimeApi::new(0, 9);

pub type WorkloadService = WorkloadOtaService<EspFlashAccess>;

#[cfg(all(feature = "test-probe-runtime", feature = "workload-native"))]
compile_error!("test-probe-runtime (test backend) and workload-native (production runtime) are two different runtimes: build the probe with --no-default-features --features test-probe-runtime");

#[cfg(feature = "test-probe-runtime")]
pub mod probe_runtime;
#[cfg(feature = "workload-native")]
pub mod native_runtime;

#[cfg(feature = "test-probe-runtime")]
pub type Supervisor =
    iobewi_esp_workload::engine::supervisor::WorkloadSupervisor<EspFlashAccess, probe_runtime::ProbeRuntime>;
#[cfg(feature = "workload-native")]
pub type Supervisor =
    iobewi_esp_workload::engine::supervisor::WorkloadSupervisor<EspFlashAccess, native_runtime::Runtime>;
/// What the HTTP routes call for activate/confirm/rollback.
#[cfg(any(feature = "test-probe-runtime", feature = "workload-native"))]
pub type Control = &'static Supervisor;
#[cfg(not(any(feature = "test-probe-runtime", feature = "workload-native")))]
pub type Control = iobewi_workload_ota_http::NoSupervisor;

static SERVICE: StaticCell<WorkloadService> = StaticCell::new();
#[cfg(any(feature = "test-probe-runtime", feature = "workload-native"))]
static SUPERVISOR: StaticCell<Supervisor> = StaticCell::new();

/// Probes the partition table, builds the (single) Workload OTA service (and the
/// Supervisor when the build has a runtime) and logs the capability and the OTM2
/// recovery state. It does **not** start any Workload: that is `reconcile_task`.
pub async fn init(
    flash: &'static SharedFlash,
    #[cfg_attr(not(feature = "test-probe-runtime"), allow(unused_variables))] spawner: embassy_executor::Spawner,
    #[cfg(feature = "workload-native")] cpu: esp_hal::system::CpuControl<'static>,
    #[cfg(feature = "workload-native")] region_ok: bool,
) -> (&'static WorkloadService, Control) {
    let capability = probe(flash).await;
    match &capability {
        Capability::Supported(layout) => log::info!(
            "workload: storage supported, meta@{:#x} slots {} B each (A@{:#x} B@{:#x})",
            layout.meta().offset,
            layout.max_artifact_size(),
            layout.slot(iobewi_esp_workload::model::Side::A).offset,
            layout.slot(iobewi_esp_workload::model::Side::B).offset,
        ),
        Capability::Unsupported(why) => {
            log::info!("workload: storage unsupported ({why:?}): Workload OTA unavailable");
        }
        Capability::TableUnreadable => log::warn!("workload: partition table unreadable: Workload OTA unavailable"),
    }
    let service: &'static WorkloadService =
        SERVICE.init(WorkloadOtaService::new(availability(flash, capability), RUNTIME_API));
    if let Ok(storage) = service.storage() {
        match storage.recover().await {
            Ok(state) => log::info!("workload: otm2 recovery: {state:?}"),
            Err(e) => log::warn!("workload: otm2 metadata unreadable: {e:?}"),
        }
    }
    #[cfg(feature = "test-probe-runtime")]
    let control: Control = {
        log::warn!("workload: supervisor probe backend ENABLED (validation build, not a production runtime)");
        &*SUPERVISOR.init(iobewi_esp_workload::engine::supervisor::WorkloadSupervisor::new(
            service,
            probe_runtime::ProbeRuntime::new(spawner),
        ))
    };
    #[cfg(feature = "workload-native")]
    let control: Control = {
        log::warn!("workload: NATIVE runtime enabled (trusted native Workloads on the second core, not isolated)");
        &*SUPERVISOR.init(iobewi_esp_workload::engine::supervisor::WorkloadSupervisor::new(
            service,
            native_runtime::new(cpu, RUNTIME_API, region_ok),
        ))
    };
    #[cfg(not(any(feature = "test-probe-runtime", feature = "workload-native")))]
    let control: Control = iobewi_workload_ota_http::NoSupervisor;
    (service, control)
}

/// The guard of dual OTA: a *new* Agent may only be confirmed if it can still run the
/// Workload that is active. `true` when there is no active Workload (or Workload OTA
/// is unsupported / unreadable: nothing to protect).
pub async fn agent_can_run_active_workload(service: &WorkloadService) -> bool {
    match service.status().await.active {
        Some(active) => RUNTIME_API.satisfies(active.requires),
        None => true,
    }
}

/// Boot-time reconciliation: OTM2 state -> running Workload (offline: needs no Wi-Fi,
/// no Core). Runs once, right after the Agent decided to stay.
#[cfg(any(feature = "test-probe-runtime", feature = "workload-native"))]
#[embassy_executor::task]
pub async fn reconcile_task(supervisor: &'static Supervisor) {
    // Crash-loop guard (native runtime): a Workload that keeps taking the whole chip down is
    // not auto-started again; the Agent stays up and a replacement can be deployed.
    #[cfg(feature = "workload-native")]
    match native_runtime::boot_decision() {
        iobewi_esp_workload::native::boot_guard::BootDecision::Start { attempts } => {
            log::info!("workload: boot auto-start {attempts}/{}", iobewi_esp_workload::native::boot_guard::MAX_UNCLEAN_STARTS);
        }
        iobewi_esp_workload::native::boot_guard::BootDecision::Suppressed { attempts } => {
            log::error!(
                "workload: {attempts} consecutive unclean boots since the Workload was last healthy: auto-start SUPPRESSED, \
                 the Agent stays up (deploy a replacement, or reboot on purpose to retry)"
            );
            return;
        }
    }
    let outcome = supervisor.reconcile_boot().await;
    log::info!("workload: boot reconcile -> {outcome:?}");
}

/// The same Bearer policy as the Agent OTA (no new auth mechanism).
pub struct WorkloadAuth<B: 'static> {
    pub agent_config: &'static crate::agent::AgentConfigSpace<B>,
}

impl<B: 'static> Clone for WorkloadAuth<B> {
    fn clone(&self) -> Self {
        Self { agent_config: self.agent_config }
    }
}

impl<B: ConfigBackend + 'static> Authorize for WorkloadAuth<B> {
    async fn authorize(&self, token: &str) -> bool {
        crate::agent::is_authorized(self.agent_config, token).await
    }
}

#[cfg(feature = "workload-selftest")]
pub mod selftest;
