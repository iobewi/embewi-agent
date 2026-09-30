//! Application service supervisors.
//!
//! Runtime and one-shot provisioning are deliberately different owners.
//! Neither transport manager nor IOBEWI OTA knows application service policy.

use embassy_executor::Spawner;
use embassy_net::Stack;

use iobewi_config_space::{ConfigBackend, ConfigSpace};
use iobewi_device::{DeviceIdentity, DeviceMetadata};
use iobewi_indicator::StatusIndicatorCapabilities;
use iobewi_esp_config_space::NvsConfigBackend;
use iobewi_esp_device::{EspDeviceIdentity, EspDeviceMetadata};
use iobewi_esp_entropy::EspEntropySource;
use iobewi_esp_indicator::EspStatusIndicator;
use iobewi_esp_flash::SharedFlash;
use iobewi_esp_https::EspTlsListener;
use iobewi_esp_log_stream::EspLogTransport;
use iobewi_esp_reboot::EspReboot;
use iobewi_esp_runtime::EspRuntimeDiagnostics;
use iobewi_esp_ota::service::{EspBoot, EspUploadWriter};
use iobewi_esp_tls::service::EspClientTransport;
use iobewi_ota::config_space::ConfigSpaceMetadataStore;
use iobewi_ota::http::{ActivateFailure, BeginError, ControlBackend, PrepareRequest, PrepareResponse, WriteBackend, WriteFinishError, WriteFinishOk};
use iobewi_ota::metadata::SessionParams;
use iobewi_ota::OtaPlatformMetadata;
use iobewi_esp_ota::EspOtaPlatformMetadata;
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
        alloc::string::String::from(iobewi_esp_ota::shared_flash::active_slot(self.flash).await)
    }

    async fn boot_info(&self) -> crate::agent::BootSnapshot {
        let boot = iobewi_esp_ota::shared_flash::boot_info(self.flash).await;
        crate::agent::BootSnapshot { slot: boot.slot, seq: boot.seq, state: boot.state }
    }
}

/// IOBEWI OTA owns the upload session and the publish-on-verified-finish
/// rule; the ESP adapter supplies only the writer that programs the
/// inactive slot. One process-wide composition instance, shared by every
/// `AgentOtaBackend` regardless of its generic config-space parameters.
static OTA_UPLOAD: iobewi_ota::service::upload::UploadManager<EspUploadWriter> =
    iobewi_ota::service::upload::UploadManager::new();

/// Bind the portable IOBEWI OTA HTTP upload service to the agent's
/// authorization policy and the ESP OTA adapter. A composition adapter
/// (`http::api::serve`'s injected `ControlBackend + WriteBackend`), not
/// application logic -- the portable HTTP layer never constructs this
/// itself, only mounts whatever it's handed. Composes `iobewi_ota::service`
/// and `iobewi_esp_ota` directly: no intermediate `ota.rs` wrapper.
pub struct AgentOtaBackend<AB: 'static, OB: 'static> {
    pub flash: &'static SharedFlash,
    pub ota_config: &'static crate::ota::OtaConfigSpace<OB>,
    pub agent_config: &'static crate::agent::AgentConfigSpace<AB>,
}

impl<AB: 'static, OB: 'static> Clone for AgentOtaBackend<AB, OB> {
    fn clone(&self) -> Self {
        Self { flash: self.flash, ota_config: self.ota_config, agent_config: self.agent_config }
    }
}

impl<AB: ConfigBackend + 'static, OB: ConfigBackend + 'static> ControlBackend for AgentOtaBackend<AB, OB> {
    async fn prepare(&self, request: &PrepareRequest) -> PrepareResponse {
        match iobewi_ota::service::prepare(
            &ConfigSpaceMetadataStore(self.ota_config),
            &EspBoot(self.flash),
            &request.chip,
            &request.partition_layout,
            ESP_DEVICE_METADATA.chip_name(),
            iobewi_esp_ota::PARTITION_LAYOUT,
            u64::from(request.size),
        ).await {
            Ok(target) => PrepareResponse::accept(target),
            Err(reason) => PrepareResponse::refuse(reason),
        }
    }

    /// `POST /v1alpha1/ota/activate` (contrat §4): points the bootloader at
    /// the staged slot and arms `OtaImageState::New` (which it promotes to
    /// `PendingVerify` on the next boot). Reads the target slot from the
    /// persisted ConfigSpace `staged` state, not the in-RAM write session --
    /// matches `firmware-c`'s own fallback ("Reprise après reboot de
    /// l'agent entre write et activate"), and works identically whether or
    /// not this device rebooted since `/ota/write` finished.
    async fn activate(&self, deployment_id: &str) -> Result<alloc::string::String, ActivateFailure> {
        let slot = iobewi_ota::service::activate_staged(
            &ConfigSpaceMetadataStore(self.ota_config), &EspBoot(self.flash), deployment_id,
        ).await.map_err(|error| match error {
            iobewi_ota::service::ActivateError::NotStaged => ActivateFailure::NotStaged,
            iobewi_ota::service::ActivateError::DeploymentMismatch => ActivateFailure::DeploymentMismatch,
            iobewi_ota::service::ActivateError::Storage(_) => ActivateFailure::Storage,
        })?;
        log::info!("ota: activate dep={deployment_id} -> slot={slot} prêt, reboot imminent");
        Ok(slot)
    }
}

impl<AB: ConfigBackend + 'static, OB: ConfigBackend + 'static> WriteBackend for AgentOtaBackend<AB, OB> {
    async fn authorize(&self, token: &str) -> bool {
        crate::agent::is_authorized(self.agent_config, token).await
    }

    async fn in_progress(&self) -> bool {
        OTA_UPLOAD.in_progress().await
    }

    async fn received(&self) -> u32 {
        OTA_UPLOAD.received().await
    }

    async fn written(&self) -> u32 {
        OTA_UPLOAD.written().await
    }

    async fn params_match(&self, params: &SessionParams) -> bool {
        OTA_UPLOAD.params_match(params).await
    }

    async fn begin(&self, params: SessionParams) -> Result<(), BeginError> {
        // Target selection releases the physical flash lock before the
        // portable service touches ConfigSpace: both capabilities share
        // that same lock.
        let target = iobewi_esp_ota::shared_flash::write_target(self.flash).await.map_err(|_| BeginError::Busy)?;
        OTA_UPLOAD.begin(
            &ConfigSpaceMetadataStore(self.ota_config), params, target.size as u64,
            EspUploadWriter::new(self.flash, target),
        ).await.map_err(|error| match error {
            iobewi_ota::service::upload::StartError::TooLarge => BeginError::TooLarge,
            iobewi_ota::service::upload::StartError::Busy => BeginError::Busy,
            iobewi_ota::service::upload::StartError::Conflict => BeginError::Conflict,
            iobewi_ota::service::upload::StartError::Storage(_) => BeginError::Storage,
        })
    }

    async fn chunk(&self, bytes: &[u8]) -> bool {
        OTA_UPLOAD.chunk(bytes).await
    }

    async fn finish(&self) -> Result<WriteFinishOk, WriteFinishError> {
        let result = OTA_UPLOAD.finish(&ConfigSpaceMetadataStore(self.ota_config)).await.map_err(|error| match error {
            iobewi_ota::service::upload::FinishError::NotWriting => WriteFinishError::NotWriting,
            iobewi_ota::service::upload::FinishError::DigestMismatch(computed) => {
                log::warn!("ota: digest mismatch, calculated={}", iobewi_ota::metadata::format_digest(&computed));
                WriteFinishError::DigestMismatch
            }
            iobewi_ota::service::upload::FinishError::Incomplete { durable } => {
                log::warn!("ota: incomplete session at {durable} bytes");
                WriteFinishError::Incomplete
            }
            iobewi_ota::service::upload::FinishError::Backend => WriteFinishError::Incomplete,
            iobewi_ota::service::upload::FinishError::Storage(_) => WriteFinishError::Storage,
        })?;
        log::info!(
            "ota: write OK {} octets ({} secteurs programmés, {} blocs erase de {} KiB) en {}ms slot={} -> staged=written",
            result.written, result.stats.sectors_flushed, result.stats.erase_batches,
            result.stats.erase_batch_kib, result.elapsed_ms, result.slot,
        );
        Ok(WriteFinishOk { written: result.written, digest: result.digest })
    }
}

/// Composition adapter for the factory-provisioning HTTP page's
/// `http::config::FactoryOta` port: verifies/stages the preloaded factory
/// agent image and activates it, composing `iobewi_ota::service` and
/// `iobewi_esp_ota` directly. The HTTP page never sees
/// `SharedFlash`/`OtaConfigSpace`, only whether each step succeeded.
#[derive(Clone, Copy)]
struct EspFactoryOta {
    flash: &'static SharedFlash,
    ota_config: &'static crate::ota::OtaConfigSpace<NvsConfigBackend>,
}

impl crate::http::config::FactoryOta for EspFactoryOta {
    /// Verifies the already-programmed inactive slot and publishes it as a
    /// normal IOBEWI OTA staged transaction. No alternate OTA/write path
    /// exists: factory flashing merely placed the bytes there ahead of time.
    async fn stage_preloaded(&self, image: crate::ota::PreloadedAgent) -> Result<(), ()> {
        let expected = iobewi_ota::metadata::parse_digest(image.digest).ok_or(())?;
        let (slot, computed) = iobewi_esp_ota::shared_flash::hash_preloaded(self.flash, image.size)
            .await
            .map_err(|_| ())?;
        if computed != expected {
            return Err(());
        }
        iobewi_ota::service::publish(
            &ConfigSpaceMetadataStore(self.ota_config),
            alloc::string::String::from(image.deployment_id),
            iobewi_ota::Committed { size: u64::from(image.size), digest: expected },
            alloc::string::String::from(slot.as_str()),
        ).await.map_err(|_| ())
    }

    async fn activate(&self, deployment_id: &str) -> Result<(), ()> {
        iobewi_ota::service::activate_staged(
            &ConfigSpaceMetadataStore(self.ota_config), &EspBoot(self.flash), deployment_id,
        ).await.map(|_| ()).map_err(|_| ())
    }
}

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

impl<I: DeviceIdentity> iobewi_log_stream::LogConfig for AgentLogConfig<I> {
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
        ESP_STATUS_INDICATOR.configurable_pins(),
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
