//! Workload OTA (S17): capability, OTM2 state and the HTTP service. Selection and
//! staging only -- nothing is loaded, executed or supervised, and activation is
//! refused (`supervisor_unavailable`) because no Workload supervisor exists yet.
//!
//! The capability comes from the on-flash partition table (partitions `wl_meta`,
//! `workload_a`, `workload_b`, found by name). A device flashed with an older table,
//! or a layout without Workload space, is `Unsupported`: the Agent never invents
//! space for it, and every Workload route answers accordingly.

use iobewi_config_space::ConfigBackend;
use iobewi_esp_flash::SharedFlash;
use iobewi_esp_workload::engine::service::WorkloadOtaService;
use iobewi_esp_workload::model::RuntimeApi;
use iobewi_esp_workload::{Capability, EspFlashAccess, availability, probe};
use iobewi_workload_ota_http::Authorize;
use static_cell::StaticCell;

/// The runtime API this Agent provides to Workloads. **Single source of truth**:
/// the Workload OTA service, `/workload/ota/status` and the activation gate all read
/// it from here. It is a compatibility contract between Agent and Workload, not a
/// kernel ABI; see `docs/dual-ota.md` in iobewi.
pub const RUNTIME_API: RuntimeApi = RuntimeApi::new(1, 0);

pub type WorkloadService = WorkloadOtaService<EspFlashAccess>;

static SERVICE: StaticCell<WorkloadService> = StaticCell::new();

/// Probes the partition table, builds the (single) Workload OTA service and logs
/// the capability and the OTM2 recovery state.
pub async fn init(flash: &'static SharedFlash) -> &'static WorkloadService {
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
    let service = SERVICE.init(WorkloadOtaService::new(availability(flash, capability), RUNTIME_API));
    if let Ok(storage) = service.storage() {
        match storage.recover().await {
            Ok(state) => log::info!("workload: otm2 recovery: {state:?}"),
            Err(e) => log::warn!("workload: otm2 metadata unreadable: {e:?}"),
        }
    }
    service
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
