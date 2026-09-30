use iobewi_config_space::ConfigBackend;
use iobewi_device::DeviceMetadata;
use iobewi_esp_config_space::NvsConfigBackend;
use iobewi_esp_device::EspDeviceMetadata;
use iobewi_esp_flash::SharedFlash;
use iobewi_esp_ota::service::{EspBoot, EspUploadWriter};
use iobewi_esp_ota::EspOtaPlatformMetadata;
use iobewi_ota::config_space::ConfigSpaceMetadataStore;
use iobewi_ota::http::{ActivateFailure, BeginError, ControlBackend, PrepareRequest, PrepareResponse, WriteBackend, WriteFinishError, WriteFinishOk};
use iobewi_ota::metadata::SessionParams;
use iobewi_ota::OtaPlatformMetadata;

static DEVICE_METADATA: EspDeviceMetadata = EspDeviceMetadata;
static OTA_METADATA: EspOtaPlatformMetadata = EspOtaPlatformMetadata;

/// Composition adapter for `agent::BootInfoSource`: converts the ESP
/// bootloader's `otadata::BootEntry` to the portable `agent::BootSnapshot`.
/// `flash` is used here only because that's what reading boot state off the
/// ESP OTA partitions actually requires, not because `/info` itself needs a
/// flash handle.
#[derive(Clone, Copy)]
pub(crate) struct AgentBootInfo {
    pub(crate) flash: &'static SharedFlash,
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
pub(crate) struct AgentOtaBackend<AB: 'static, OB: 'static> {
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
            DEVICE_METADATA.chip_name(),
            OTA_METADATA.partition_layout(),
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
pub(crate) struct EspFactoryOta {
    pub(crate) flash: &'static SharedFlash,
    pub(crate) ota_config: &'static crate::ota::OtaConfigSpace<NvsConfigBackend>,
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

