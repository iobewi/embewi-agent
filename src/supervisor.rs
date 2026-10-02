//! Application service supervisors.
//!
//! Runtime and one-shot provisioning are deliberately different owners.
//! Portable services live in IOBEWI; concrete platform bindings live in
//! `composition`. This module owns only Embewi's service-start policy.

use embassy_executor::Spawner;
use embassy_net::Stack;
use iobewi_config_space::ConfigSpace;
use iobewi_esp_config_space::NvsConfigBackend;
use iobewi_esp_flash::SharedFlash;
use crate::esp_reboot::EspReboot;
use iobewi_esp_runtime::EspRuntimeDiagnostics;

use crate::composition::tasks;

/// The storage-health view consumed by Embewi's /health endpoint. The
/// concrete backend remains selected at composition; the application only
/// consumes its local `StorageHealth` port.
impl crate::agent::StorageHealth for NvsConfigBackend {
    fn is_healthy(&self) -> bool {
        NvsConfigBackend::is_healthy(self)
    }
}

pub struct ApplicationSupervisor {
    spawner: Spawner,
    reboot: EspReboot,
    tls: crate::esp_tls::TlsReferenceStatic,
    agent_config: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
    app_config: &'static ConfigSpace<NvsConfigBackend>,
    tls_config: &'static crate::esp_tls::TlsConfigSpace<NvsConfigBackend>,
    runtime_config: &'static crate::runtime_config::RuntimeConfig<NvsConfigBackend>,
    ota_config: &'static crate::ota::OtaConfigSpace<NvsConfigBackend>,
    flash: &'static SharedFlash,
    workload: &'static crate::workload::WorkloadService,
    nvs_backend: &'static NvsConfigBackend,
    diagnostics: EspRuntimeDiagnostics,
    ip_services_started: bool,
}

impl ApplicationSupervisor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        spawner: Spawner,
        reboot: EspReboot,
        tls: crate::esp_tls::TlsReferenceStatic,
        agent_config: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
        app_config: &'static ConfigSpace<NvsConfigBackend>,
        tls_config: &'static crate::esp_tls::TlsConfigSpace<NvsConfigBackend>,
        runtime_config: &'static crate::runtime_config::RuntimeConfig<NvsConfigBackend>,
        ota_config: &'static crate::ota::OtaConfigSpace<NvsConfigBackend>,
        flash: &'static SharedFlash,
        workload: &'static crate::workload::WorkloadService,
        nvs_backend: &'static NvsConfigBackend,
        diagnostics: EspRuntimeDiagnostics,
    ) -> Self {
        Self {
            spawner,
            reboot,
            tls,
            agent_config,
            app_config,
            tls_config,
            runtime_config,
            ota_config,
            flash,
            workload,
            nvs_backend,
            diagnostics,
            ip_services_started: false,
        }
    }

    pub fn on_ip_ready(&mut self, stack: Stack<'static>) {
        if self.ip_services_started {
            log::info!("supervisor: runtime IP services already started");
            return;
        }
        self.ip_services_started = true;

        self.spawner
            .spawn(tasks::run_http_api(
                stack,
                self.flash,
                self.nvs_backend,
                self.agent_config,
                self.app_config,
                self.tls_config,
                self.runtime_config,
                self.ota_config,
                self.workload,
                self.reboot.clone(),
                self.tls,
            ).unwrap());

        self.spawner.spawn(crate::time::sync_task(stack, crate::time::SyncOptions {
            server: "pool.ntp.org",
            resync_period: embassy_time::Duration::from_secs(3600),
            retry_period: embassy_time::Duration::from_secs(15),
            exchange_timeout: embassy_time::Duration::from_secs(10),
            plausible_epoch_floor: 1_700_000_000,
        }).unwrap());
        self.spawner
            .spawn(tasks::run_heartbeat(
                stack,
                self.agent_config,
                self.runtime_config,
                self.ota_config,
                self.tls_config,
                self.tls,
                self.diagnostics,
            ).unwrap());
        self.spawner
            .spawn(tasks::run_log_stream(
                stack,
                self.agent_config,
                self.tls_config,
                self.tls,
            ).unwrap());
    }
}

/// Services available in the disposable init image after USB/Improv has
/// produced an IP capability. Only the HTTPS provisioning UI is started.
pub struct ProvisioningSupervisor {
    spawner: Spawner,
    reboot: EspReboot,
    tls: crate::esp_tls::TlsReferenceStatic,
    flash: &'static SharedFlash,
    agent_config: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
    hardware_config: &'static crate::hardware::HardwareConfigSpace<NvsConfigBackend>,
    tls_config: &'static crate::esp_tls::TlsConfigSpace<NvsConfigBackend>,
    lifecycle_config: &'static crate::ota::BootstrapConfigSpace<NvsConfigBackend>,
    ota_config: &'static crate::ota::OtaConfigSpace<NvsConfigBackend>,
    factory_agent: crate::ota::PreloadedAgent,
    ip_services_started: bool,
}

impl ProvisioningSupervisor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        spawner: Spawner,
        reboot: EspReboot,
        tls: crate::esp_tls::TlsReferenceStatic,
        flash: &'static SharedFlash,
        agent_config: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
        hardware_config: &'static crate::hardware::HardwareConfigSpace<NvsConfigBackend>,
        tls_config: &'static crate::esp_tls::TlsConfigSpace<NvsConfigBackend>,
        lifecycle_config: &'static crate::ota::BootstrapConfigSpace<NvsConfigBackend>,
        ota_config: &'static crate::ota::OtaConfigSpace<NvsConfigBackend>,
        factory_agent: crate::ota::PreloadedAgent,
    ) -> Self {
        Self {
            spawner,
            reboot,
            tls,
            flash,
            agent_config,
            hardware_config,
            tls_config,
            lifecycle_config,
            ota_config,
            factory_agent,
            ip_services_started: false,
        }
    }

}

impl crate::provisioning::NetworkReady<Stack<'static>> for ProvisioningSupervisor {
    fn on_network_ready(&mut self, network: Stack<'static>) {
        if self.ip_services_started {
            log::info!("supervisor: provisioning HTTPS already started");
            return;
        }
        self.ip_services_started = true;
        self.spawner
            .spawn(tasks::run_http_provisioning(
                network,
                self.flash,
                self.agent_config,
                self.hardware_config,
                self.tls_config,
                self.lifecycle_config,
                self.ota_config,
                self.factory_agent,
                self.reboot.clone(),
                self.tls,
            ).unwrap());
    }
}
