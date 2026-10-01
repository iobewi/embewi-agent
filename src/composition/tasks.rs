use embassy_net::Stack;
use iobewi_config_space::ConfigSpace;
use iobewi_device::DeviceIdentity;
use iobewi_esp_config_space::NvsConfigBackend;
use iobewi_esp_device::{EspDeviceIdentity, EspDeviceMetadata};
use iobewi_esp_entropy::EspEntropySource;
use iobewi_esp_flash::SharedFlash;
use iobewi_esp_ota::EspOtaPlatformMetadata;
use iobewi_esp_https::EspTlsListener;
use iobewi_esp_indicator::EspStatusIndicator;
use iobewi_esp_log_stream::EspLogEntropy;
use iobewi_esp_reboot::EspReboot;
use iobewi_esp_runtime::EspRuntimeDiagnostics;
use iobewi_esp_tls::service::EspClientTransport;
use static_cell::StaticCell;

use super::ota::{AgentBootInfo, AgentOtaBackend, EspFactoryOta};
use super::tls::AgentTlsProvisioningBackend;

/// Process-wide handles for portable IOBEWI capabilities supplied by the
/// selected ESP platform adapter. Embewi consumes only the IOBEWI traits;
/// the concrete ESP types are chosen here at composition.
static ESP_DEVICE_IDENTITY: EspDeviceIdentity = EspDeviceIdentity;
static ESP_DEVICE_METADATA: EspDeviceMetadata = EspDeviceMetadata;
static ESP_OTA_METADATA: EspOtaPlatformMetadata = EspOtaPlatformMetadata;
static ESP_ENTROPY: EspEntropySource = EspEntropySource;
static ESP_STATUS_INDICATOR: EspStatusIndicator = EspStatusIndicator;

struct AgentLogConfig<I: 'static> {
    space: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
    identity: &'static I,
}

impl<I: DeviceIdentity> iobewi_log::LogMetadata for AgentLogConfig<I> {
    async fn node_id(&self) -> alloc::string::String { crate::agent::node_id(self.space, self.identity).await }
    fn timestamp(&self) -> u64 { crate::time::now().unwrap_or(0) }
    fn workload(&self) -> &'static str { crate::agent::FW_NAME }
}

impl<I: DeviceIdentity> iobewi_log_stream::StreamConfig for AgentLogConfig<I> {
    async fn ctrl_url(&self) -> alloc::string::String { crate::agent::ctrl_url(self.space).await }
    async fn token(&self) -> alloc::string::String { crate::agent::token(self.space).await }
    fn path(&self) -> alloc::string::String {
        alloc::format!("{}/logs", crate::http::api::API_PREFIX)
    }
}

/// Constructs the platform's `SecureClientTransport` and calls into the
/// portable, generic `heartbeat::run` -- embassy tasks can't themselves be
/// generic, so the platform's concrete transport type is chosen here, at
/// the composition root, exactly like `run_log_stream` just below.
#[embassy_executor::task]
pub(crate) async fn run_heartbeat(
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
pub(crate) async fn run_log_stream(
    stack: Stack<'static>,
    agent_config: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
    tls_config: &'static iobewi_esp_tls::service::TlsConfigSpace<NvsConfigBackend>,
    tls: iobewi_esp_tls::service::TlsReferenceStatic,
) -> ! {
    let config = AgentLogConfig { space: agent_config, identity: &ESP_DEVICE_IDENTITY };
    let transport = EspClientTransport { tls, stack, tls_config, clock_is_set: crate::time::is_set };
    iobewi_log_stream::run(&config, &transport, &EspLogEntropy).await
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
pub(crate) async fn run_http_api(
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
    let ota_backend = AgentOtaBackend { flash, ota_config, agent_config };
    crate::http::api::serve(
        &mut listener,
        ota_backend,
        nvs_backend,
        agent_config,
        app_config,
        runtime_config,
        ota_config,
        reboot,
        tls_backend,
        boot,
        &ESP_DEVICE_METADATA,
        &ESP_OTA_METADATA,
        &ESP_DEVICE_IDENTITY,
    ).await
}

/// Same composition role as [`run_http_api`], for the disposable init
/// image's provisioning surface.
#[embassy_executor::task]
pub(crate) async fn run_http_provisioning(
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
    let factory_ota = EspFactoryOta { flash, ota_config };
    crate::http::config::serve(
        &mut listener,
        agent_config,
        hardware_config,
        lifecycle_config,
        factory_ota,
        factory_agent,
        reboot,
        &ESP_DEVICE_IDENTITY,
        &ESP_ENTROPY,
        &ESP_STATUS_INDICATOR,
    ).await
}

