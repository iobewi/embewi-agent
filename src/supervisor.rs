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

/// Last 3 bytes of the efuse-burned MAC address: the device-unique suffix
/// every ESP composition-root identity/name derived from hardware uses.
/// Shared with `bin/init.rs`'s Improv device name to avoid two independent
/// reads of the same efuse, even though the two names built from it
/// (`embewi-xxxxxx` vs `embewi-init-xxxxxx`) stay distinct application
/// concerns.
pub fn mac_suffix() -> [u8; 3] {
    let mac = esp_hal::efuse::base_mac_address();
    let mac = mac.as_bytes();
    [mac[3], mac[4], mac[5]]
}

/// ZST adapter for `agent::DeviceIdentity`: the fallback `node_id` used
/// until one is persisted. Produces exactly the same `embewi-xxxxxx` shape
/// as before this capability existed.
#[derive(Clone, Copy)]
pub struct EspDeviceIdentity;

impl crate::agent::DeviceIdentity for EspDeviceIdentity {
    fn fallback_node_id(&self) -> alloc::string::String {
        let mac = mac_suffix();
        alloc::format!("embewi-{:02x}{:02x}{:02x}", mac[0], mac[1], mac[2])
    }
}

/// ZST adapter for `agent::TokenEntropy`. Same hardware RNG source used
/// today -- not the ADC-backed `TrngSource` used for the pre-Wi-Fi TLS
/// bootstrap identity, which stays its own, separate policy.
#[derive(Clone, Copy)]
pub struct EspTokenEntropy;

impl crate::agent::TokenEntropy for EspTokenEntropy {
    fn fill_random(&self, output: &mut [u8]) {
        esp_hal::rng::Rng::new().read(output);
    }
}

/// ZST adapter for `agent::DeviceMetadata`.
#[derive(Clone, Copy)]
pub struct EspDeviceMetadata;

impl crate::agent::DeviceMetadata for EspDeviceMetadata {
    fn chip_name(&self) -> &'static str {
        esp_metadata_generated::chip_pretty!()
    }

    fn ram_size(&self) -> u32 {
        let dram = esp_metadata_generated::memory_range!("DRAM");
        (dram.end - dram.start) as u32
    }
}

/// One process-wide instance of each ZST platform capability -- there is no
/// state to construct, so a plain `static` gives every task a `'static`
/// reference without a `StaticCell`.
static ESP_DEVICE_IDENTITY: EspDeviceIdentity = EspDeviceIdentity;
static ESP_TOKEN_ENTROPY: EspTokenEntropy = EspTokenEntropy;
static ESP_DEVICE_METADATA: EspDeviceMetadata = EspDeviceMetadata;

struct AgentLogConfig<I: 'static> {
    space: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
    identity: &'static I,
}

impl<I: crate::agent::DeviceIdentity> iobewi_log_stream::LogConfig for AgentLogConfig<I> {
    async fn ctrl_url(&self) -> alloc::string::String { crate::agent::ctrl_url(self.space).await }
    async fn token(&self) -> alloc::string::String { crate::agent::token(self.space).await }
    async fn node_id(&self) -> alloc::string::String { crate::agent::node_id(self.space, self.identity).await }
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
    agent_config: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
    runtime_config: &'static crate::runtime_config::RuntimeConfig<NvsConfigBackend>,
    ota_config: &'static crate::ota::OtaConfigSpace<NvsConfigBackend>,
    tls_config: &'static iobewi_esp_tls::service::TlsConfigSpace<NvsConfigBackend>,
    tls: iobewi_esp_tls::service::TlsReferenceStatic,
    diagnostics: EspRuntimeDiagnostics,
) -> ! {
    static TRANSPORT: StaticCell<EspClientTransport<NvsConfigBackend>> = StaticCell::new();
    let transport = &*TRANSPORT.init(EspClientTransport { tls, stack, tls_config, clock_is_set: crate::time::is_set });
    crate::heartbeat::run(transport, diagnostics, &ESP_DEVICE_IDENTITY, agent_config, runtime_config, ota_config).await
}

#[embassy_executor::task]
async fn run_log_stream(
    stack: Stack<'static>,
    agent_config: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
    tls_config: &'static iobewi_esp_tls::service::TlsConfigSpace<NvsConfigBackend>,
    tls: iobewi_esp_tls::service::TlsReferenceStatic,
) -> ! {
    let config = AgentLogConfig { space: agent_config, identity: &ESP_DEVICE_IDENTITY };
    let transport = EspLogTransport { stack, tls, tls_config, clock_is_set: crate::time::is_set };
    iobewi_log_stream::run(&config, &transport).await
}

/// Application authorization plus ESP persistence for the portable TLS
/// provisioning API (`iobewi_tls::http::ProvisioningBackend`). Built once at
/// composition and injected into `http::api::serve` -- the portable HTTP
/// layer never sees `iobewi_esp_tls` or `TlsConfigSpace` itself.
#[derive(Clone, Copy)]
struct AgentTlsProvisioningBackend {
    tls_config: &'static iobewi_esp_tls::service::TlsConfigSpace<NvsConfigBackend>,
    agent_config: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
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
    agent_config: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
    app_config: &'static ConfigSpace<NvsConfigBackend>,
    tls_config: &'static iobewi_esp_tls::service::TlsConfigSpace<NvsConfigBackend>,
    runtime_config: &'static crate::runtime_config::RuntimeConfig<NvsConfigBackend>,
    ota_config: &'static crate::ota::OtaConfigSpace<NvsConfigBackend>,
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
        &ESP_DEVICE_METADATA,
        &ESP_DEVICE_IDENTITY,
    ).await
}

/// Same composition role as [`run_http_api`], for the disposable init
/// image's provisioning surface.
#[embassy_executor::task]
async fn run_http_provisioning(
    stack: Stack<'static>,
    flash: &'static SharedFlash,
    agent_config: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
    hardware_config: &'static crate::hardware::HardwareConfigSpace<NvsConfigBackend>,
    tls_config: &'static iobewi_esp_tls::service::TlsConfigSpace<NvsConfigBackend>,
    lifecycle_config: &'static crate::ota::BootstrapConfigSpace<NvsConfigBackend>,
    ota_config: &'static crate::ota::OtaConfigSpace<NvsConfigBackend>,
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
        &ESP_DEVICE_IDENTITY,
        &ESP_TOKEN_ENTROPY,
    ).await
}

pub struct ApplicationSupervisor {
    spawner: Spawner,
    reboot: EspReboot,
    tls: iobewi_esp_tls::service::TlsReferenceStatic,
    agent_config: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
    app_config: &'static ConfigSpace<NvsConfigBackend>,
    tls_config: &'static iobewi_esp_tls::service::TlsConfigSpace<NvsConfigBackend>,
    runtime_config: &'static crate::runtime_config::RuntimeConfig<NvsConfigBackend>,
    ota_config: &'static crate::ota::OtaConfigSpace<NvsConfigBackend>,
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
        agent_config: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
        app_config: &'static ConfigSpace<NvsConfigBackend>,
        tls_config: &'static iobewi_esp_tls::service::TlsConfigSpace<NvsConfigBackend>,
        runtime_config: &'static crate::runtime_config::RuntimeConfig<NvsConfigBackend>,
        ota_config: &'static crate::ota::OtaConfigSpace<NvsConfigBackend>,
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
    agent_config: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
    hardware_config: &'static crate::hardware::HardwareConfigSpace<NvsConfigBackend>,
    tls_config: &'static iobewi_esp_tls::service::TlsConfigSpace<NvsConfigBackend>,
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
        tls: iobewi_esp_tls::service::TlsReferenceStatic,
        flash: &'static SharedFlash,
        agent_config: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
        hardware_config: &'static crate::hardware::HardwareConfigSpace<NvsConfigBackend>,
        tls_config: &'static iobewi_esp_tls::service::TlsConfigSpace<NvsConfigBackend>,
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
