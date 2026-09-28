//! Application service supervisors.
//!
//! Runtime and one-shot provisioning are deliberately different owners.
//! Neither transport manager nor IOBEWI OTA knows application service policy.

use embassy_executor::Spawner;
use embassy_net::Stack;
use esp_hal::peripherals::LPWR;
use log::warn;

use iobewi_esp_config_space::NvsConfigBackend;
use iobewi_esp_flash::SharedFlash;

pub struct ApplicationSupervisor {
    spawner: Spawner,
    lpwr: Option<LPWR<'static>>,
    tls: crate::tls::TlsReferenceStatic,
    agent_config: &'static crate::agent::AgentConfigSpace,
    app_config: &'static crate::app_config::AppConfigSpace,
    tls_config: &'static crate::tls::TlsConfigSpace,
    runtime_config: &'static crate::runtime_config::RuntimeConfig,
    ota_config: &'static crate::ota::OtaConfigSpace,
    flash: &'static SharedFlash,
    nvs_backend: &'static NvsConfigBackend,
    ip_services_started: bool,
}

impl ApplicationSupervisor {
    pub fn new(
        spawner: Spawner,
        lpwr: LPWR<'static>,
        tls: crate::tls::TlsReferenceStatic,
        agent_config: &'static crate::agent::AgentConfigSpace,
        app_config: &'static crate::app_config::AppConfigSpace,
        tls_config: &'static crate::tls::TlsConfigSpace,
        runtime_config: &'static crate::runtime_config::RuntimeConfig,
        ota_config: &'static crate::ota::OtaConfigSpace,
        flash: &'static SharedFlash,
        nvs_backend: &'static NvsConfigBackend,
    ) -> Self {
        Self {
            spawner,
            lpwr: Some(lpwr),
            tls,
            agent_config,
            app_config,
            tls_config,
            runtime_config,
            ota_config,
            flash,
            nvs_backend,
            ip_services_started: false,
        }
    }

    pub fn on_ip_ready(&mut self, stack: Stack<'static>) {
        if self.ip_services_started {
            log::info!("supervisor: runtime IP services already started");
            return;
        }
        let Some(lpwr) = self.lpwr.take() else {
            warn!("supervisor: runtime IP services requested without LPWR");
            return;
        };
        self.ip_services_started = true;

        self.spawner
            .spawn(crate::http::run(
                stack,
                self.flash,
                self.nvs_backend,
                self.agent_config,
                self.app_config,
                self.tls_config,
                self.runtime_config,
                self.ota_config,
                self.spawner,
                lpwr,
                self.tls,
            ).unwrap());

        self.spawner.spawn(crate::time::sync_task(stack).unwrap());
        self.spawner
            .spawn(crate::heartbeat::run(
                stack,
                self.agent_config,
                self.runtime_config,
                self.ota_config,
                self.tls_config,
                self.tls,
            ).unwrap());
        self.spawner
            .spawn(crate::log_stream::run(
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
    lpwr: Option<LPWR<'static>>,
    tls: crate::tls::TlsReferenceStatic,
    flash: &'static SharedFlash,
    agent_config: &'static crate::agent::AgentConfigSpace,
    hardware_config: &'static crate::hardware::HardwareConfigSpace,
    tls_config: &'static crate::tls::TlsConfigSpace,
    lifecycle_config: &'static crate::lifecycle::LifecycleConfigSpace,
    ota_config: &'static crate::ota::OtaConfigSpace,
    factory_agent: crate::ota::PreloadedAgent,
    ip_services_started: bool,
}

impl ProvisioningSupervisor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        spawner: Spawner,
        lpwr: LPWR<'static>,
        tls: crate::tls::TlsReferenceStatic,
        flash: &'static SharedFlash,
        agent_config: &'static crate::agent::AgentConfigSpace,
        hardware_config: &'static crate::hardware::HardwareConfigSpace,
        tls_config: &'static crate::tls::TlsConfigSpace,
        lifecycle_config: &'static crate::lifecycle::LifecycleConfigSpace,
        ota_config: &'static crate::ota::OtaConfigSpace,
        factory_agent: crate::ota::PreloadedAgent,
    ) -> Self {
        Self {
            spawner,
            lpwr: Some(lpwr),
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

    pub fn on_ip_ready(&mut self, stack: Stack<'static>) {
        if self.ip_services_started {
            log::info!("supervisor: provisioning HTTPS already started");
            return;
        }
        let Some(lpwr) = self.lpwr.take() else {
            warn!("supervisor: provisioning requested without LPWR");
            return;
        };
        self.ip_services_started = true;
        self.spawner
            .spawn(crate::http::run_provisioning(
                stack,
                self.flash,
                self.agent_config,
                self.hardware_config,
                self.tls_config,
                self.lifecycle_config,
                self.ota_config,
                self.factory_agent,
                self.spawner,
                lpwr,
                self.tls,
            ).unwrap());
    }
}
