//! Application service supervisors.
//!
//! Runtime and one-shot provisioning are deliberately different owners.
//! Neither transport manager nor IOBEWI OTA knows application service policy.

use embassy_executor::Spawner;
use embassy_net::Stack;

use iobewi_config_space::ConfigSpace;
use iobewi_esp_config_space::NvsConfigBackend;
use iobewi_esp_flash::SharedFlash;
use iobewi_esp_https::EspTlsListener;
use iobewi_esp_log_stream::EspLogTransport;
use iobewi_esp_reboot::EspReboot;
use iobewi_esp_runtime::EspRuntimeDiagnostics;
use iobewi_esp_tls::service::EspClientTransport;
use static_cell::StaticCell;

/// The trait is local to `embewi-agent` (`agent::StorageHealth`), so
/// implementing it for this foreign ESP type respects the orphan rule and
/// needs no wrapper.
impl crate::agent::StorageHealth for NvsConfigBackend {
    fn is_healthy(&self) -> bool {
        NvsConfigBackend::is_healthy(self)
    }
}

/// Composition adapter for `agent::BootInfoSource`: converts the ESP
/// bootloader's `otadata::BootEntry` to the portable `agent::BootSnapshot`.
/// `flash` is used here only because that's what reading boot state off the
/// ESP OTA partitions actually requires, not because `/info` itself needs a
/// flash handle.
#[derive(Clone, Copy)]
struct AgentBootInfo {
    flash: &'static SharedFlash,
}

impl crate::agent::BootInfoSource for AgentBootInfo {
    async fn active_slot(&self) -> alloc::string::String {
        crate::ota::active_slot(self.flash).await
    }

    async fn boot_info(&self) -> crate::agent::BootSnapshot {
        let boot = crate::ota::boot_info(self.flash).await;
        crate::agent::BootSnapshot { slot: boot.slot, seq: boot.seq, state: boot.state }
    }
}

struct AgentLogConfig {
    space: &'static crate::agent::AgentConfigSpace,
}

impl iobewi_log_stream::LogConfig for AgentLogConfig {
    async fn ctrl_url(&self) -> alloc::string::String { crate::agent::ctrl_url(self.space).await }
    async fn token(&self) -> alloc::string::String { crate::agent::token(self.space).await }
    async fn node_id(&self) -> alloc::string::String { crate::agent::node_id(self.space).await }
    fn timestamp(&self) -> u64 { crate::time::now().unwrap_or(0) }
    fn workload(&self) -> &'static str { crate::agent::FW_NAME }
    fn path(&self) -> alloc::string::String {
        alloc::format!("{}/logs", crate::http::api::API_PREFIX)
    }
}

/// Constructs the platform's `SecureClientTransport` and calls into the
/// portable, generic `heartbeat::run` -- embassy tasks can't themselves be
/// generic, so the platform's concrete transport type is chosen here, at
/// the composition root, exactly like `run_log_stream` just below.
#[embassy_executor::task]
async fn run_heartbeat(
    stack: Stack<'static>,
    agent_config: &'static crate::agent::AgentConfigSpace,
    runtime_config: &'static crate::runtime_config::RuntimeConfig,
    ota_config: &'static crate::ota::OtaConfigSpace,
    tls_config: &'static iobewi_esp_tls::service::TlsConfigSpace,
    tls: iobewi_esp_tls::service::TlsReferenceStatic,
    diagnostics: EspRuntimeDiagnostics,
) -> ! {
    static TRANSPORT: StaticCell<EspClientTransport> = StaticCell::new();
    let transport = &*TRANSPORT.init(EspClientTransport { tls, stack, tls_config, clock_is_set: crate::time::is_set });
    crate::heartbeat::run(transport, diagnostics, agent_config, runtime_config, ota_config).await
}

#[embassy_executor::task]
async fn run_log_stream(
    stack: Stack<'static>,
    agent_config: &'static crate::agent::AgentConfigSpace,
    tls_config: &'static iobewi_esp_tls::service::TlsConfigSpace,
    tls: iobewi_esp_tls::service::TlsReferenceStatic,
) -> ! {
    let config = AgentLogConfig { space: agent_config };
    let transport = EspLogTransport { stack, tls, tls_config, clock_is_set: crate::time::is_set };
    iobewi_log_stream::run(&config, &transport).await
}

/// Application authorization plus ESP persistence for the portable TLS
/// provisioning API (`iobewi_tls::http::ProvisioningBackend`). Built once at
/// composition and injected into `http::api::serve` -- the portable HTTP
/// layer never sees `iobewi_esp_tls` or `TlsConfigSpace` itself.
#[derive(Clone, Copy)]
struct AgentTlsProvisioningBackend {
    tls_config: &'static iobewi_esp_tls::service::TlsConfigSpace,
    agent_config: &'static crate::agent::AgentConfigSpace,
}

impl iobewi_tls::http::ProvisioningBackend for AgentTlsProvisioningBackend {
    async fn authorize(&self, token: &str) -> bool {
        crate::agent::is_authorized(self.agent_config, token).await
    }

    async fn save_cert(&self, cert_pem: &str, key_pem: &str) -> Result<(), iobewi_tls::SaveCertError> {
        iobewi_esp_tls::service::save_cert(self.tls_config, cert_pem, key_pem).await
    }

    async fn save_ca(&self, ca_pem: &str) -> Result<(), iobewi_tls::SaveCertError> {
        iobewi_esp_tls::service::save_ca(self.tls_config, ca_pem).await
    }
}

/// Constructs the platform's `EspTlsListener` (the ESP implementation of the
/// portable `iobewi_https::TlsListener`) and calls into the generic,
/// portable `http::api::serve` -- embassy tasks can't themselves be
/// generic, so the platform's concrete listener type is chosen here, at the
/// composition root, exactly like `run_heartbeat`/`run_log_stream` above.
/// TLS identity management (`tls_config`) is still an ESP composition
/// concern for now; only which listener implementation backs the router has
/// moved out of the portable HTTP layer.
#[embassy_executor::task]
async fn run_http_api(
    stack: Stack<'static>,
    flash: &'static SharedFlash,
    nvs_backend: &'static NvsConfigBackend,
    agent_config: &'static crate::agent::AgentConfigSpace,
    app_config: &'static ConfigSpace<NvsConfigBackend>,
    tls_config: &'static iobewi_esp_tls::service::TlsConfigSpace,
    runtime_config: &'static crate::runtime_config::RuntimeConfig,
    ota_config: &'static crate::ota::OtaConfigSpace,
    reboot: EspReboot,
    tls: iobewi_esp_tls::service::TlsReferenceStatic,
) -> ! {
    let mut rx = [0u8; 1024];
    let mut tx = [0u8; 1024];
    let mut listener = EspTlsListener::new(
        stack,
        tls,
        || iobewi_esp_tls::service::server_config(tls_config),
        &mut rx,
        &mut tx,
    );
    let tls_backend = AgentTlsProvisioningBackend { tls_config, agent_config };
    static BOOT: StaticCell<AgentBootInfo> = StaticCell::new();
    let boot = &*BOOT.init(AgentBootInfo { flash });
    crate::http::api::serve(
        &mut listener,
        flash,
        nvs_backend,
        agent_config,
        app_config,
        runtime_config,
        ota_config,
        reboot,
        tls_backend,
        boot,
    ).await
}

/// Same composition role as [`run_http_api`], for the disposable init
/// image's provisioning surface.
#[embassy_executor::task]
async fn run_http_provisioning(
    stack: Stack<'static>,
    flash: &'static SharedFlash,
    agent_config: &'static crate::agent::AgentConfigSpace,
    hardware_config: &'static crate::hardware::HardwareConfigSpace,
    tls_config: &'static iobewi_esp_tls::service::TlsConfigSpace,
    lifecycle_config: &'static crate::ota::BootstrapConfigSpace,
    ota_config: &'static crate::ota::OtaConfigSpace,
    factory_agent: crate::ota::PreloadedAgent,
    reboot: EspReboot,
    tls: iobewi_esp_tls::service::TlsReferenceStatic,
) -> ! {
    let mut rx = [0u8; 1024];
    let mut tx = [0u8; 1024];
    let mut listener = EspTlsListener::new(
        stack,
        tls,
        || iobewi_esp_tls::service::server_config(tls_config),
        &mut rx,
        &mut tx,
    );
    crate::http::config::serve(
        &mut listener,
        flash,
        agent_config,
        hardware_config,
        lifecycle_config,
        ota_config,
        factory_agent,
        reboot,
    ).await
}

pub struct ApplicationSupervisor {
    spawner: Spawner,
    reboot: EspReboot,
    tls: iobewi_esp_tls::service::TlsReferenceStatic,
    agent_config: &'static crate::agent::AgentConfigSpace,
    app_config: &'static ConfigSpace<NvsConfigBackend>,
    tls_config: &'static iobewi_esp_tls::service::TlsConfigSpace,
    runtime_config: &'static crate::runtime_config::RuntimeConfig,
    ota_config: &'static crate::ota::OtaConfigSpace,
    flash: &'static SharedFlash,
    nvs_backend: &'static NvsConfigBackend,
    diagnostics: EspRuntimeDiagnostics,
    ip_services_started: bool,
}

impl ApplicationSupervisor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        spawner: Spawner,
        reboot: EspReboot,
        tls: iobewi_esp_tls::service::TlsReferenceStatic,
        agent_config: &'static crate::agent::AgentConfigSpace,
        app_config: &'static ConfigSpace<NvsConfigBackend>,
        tls_config: &'static iobewi_esp_tls::service::TlsConfigSpace,
        runtime_config: &'static crate::runtime_config::RuntimeConfig,
        ota_config: &'static crate::ota::OtaConfigSpace,
        flash: &'static SharedFlash,
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
            .spawn(run_http_api(
                stack,
                self.flash,
                self.nvs_backend,
                self.agent_config,
                self.app_config,
                self.tls_config,
                self.runtime_config,
                self.ota_config,
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
            .spawn(run_heartbeat(
                stack,
                self.agent_config,
                self.runtime_config,
                self.ota_config,
                self.tls_config,
                self.tls,
                self.diagnostics,
            ).unwrap());
        self.spawner
            .spawn(run_log_stream(
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
    tls: iobewi_esp_tls::service::TlsReferenceStatic,
    flash: &'static SharedFlash,
    agent_config: &'static crate::agent::AgentConfigSpace,
    hardware_config: &'static crate::hardware::HardwareConfigSpace,
    tls_config: &'static iobewi_esp_tls::service::TlsConfigSpace,
    lifecycle_config: &'static crate::ota::BootstrapConfigSpace,
    ota_config: &'static crate::ota::OtaConfigSpace,
    factory_agent: crate::ota::PreloadedAgent,
    ip_services_started: bool,
}

impl ProvisioningSupervisor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        spawner: Spawner,
        reboot: EspReboot,
        tls: iobewi_esp_tls::service::TlsReferenceStatic,
        flash: &'static SharedFlash,
        agent_config: &'static crate::agent::AgentConfigSpace,
        hardware_config: &'static crate::hardware::HardwareConfigSpace,
        tls_config: &'static iobewi_esp_tls::service::TlsConfigSpace,
        lifecycle_config: &'static crate::ota::BootstrapConfigSpace,
        ota_config: &'static crate::ota::OtaConfigSpace,
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
            .spawn(run_http_provisioning(
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
