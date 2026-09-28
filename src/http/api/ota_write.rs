//! Bind the portable IOBEWI OTA HTTP upload service to the agent's
//! authorization policy and the ESP OTA adapter.

use crate::{agent, ota};
use iobewi_esp_flash::SharedFlash;
use iobewi_ota::http::{ActivateFailure, BeginError, ControlBackend, PrepareRequest, PrepareResponse, WriteBackend, WriteFinishError, WriteFinishOk};
use iobewi_ota::metadata::SessionParams;

#[derive(Clone)]
pub struct AgentOtaBackend {
    pub flash: &'static SharedFlash,
    pub ota_config: &'static ota::OtaConfigSpace,
    pub agent_config: &'static agent::AgentConfigSpace,
}

impl ControlBackend for AgentOtaBackend {
    async fn prepare(&self, request: &PrepareRequest) -> PrepareResponse {
        ota::prepare(self.flash, self.ota_config, request).await
    }

    async fn activate(&self, deployment_id: &str) -> Result<alloc::string::String, ActivateFailure> {
        ota::activate(self.flash, self.ota_config, deployment_id).await.map_err(|error| match error {
            ota::ActivateError::NotStaged => ActivateFailure::NotStaged,
            ota::ActivateError::DeploymentMismatch => ActivateFailure::DeploymentMismatch,
            ota::ActivateError::Storage(_) => ActivateFailure::Storage,
        })
    }
}

impl WriteBackend for AgentOtaBackend {
    async fn authorize(&self, token: &str) -> bool {
        agent::is_authorized(self.agent_config, token).await
    }

    async fn in_progress(&self) -> bool {
        ota::write_in_progress().await
    }

    async fn received(&self) -> u32 {
        ota::write_received().await
    }

    async fn written(&self) -> u32 {
        ota::write_written().await
    }

    async fn params_match(&self, params: &SessionParams) -> bool {
        ota::write_params_match(params).await
    }

    async fn begin(&self, params: SessionParams) -> Result<(), BeginError> {
        ota::write_begin(self.flash, self.ota_config, params).await.map_err(|error| match error {
            ota::BeginError::Busy => BeginError::Busy,
            ota::BeginError::TooLarge => BeginError::TooLarge,
            ota::BeginError::Conflict => BeginError::Conflict,
            ota::BeginError::Storage(_) => BeginError::Storage,
        })
    }

    async fn chunk(&self, bytes: &[u8]) -> bool {
        ota::write_chunk(bytes).await
    }

    async fn finish(&self) -> Result<WriteFinishOk, WriteFinishError> {
        ota::write_finish(self.ota_config).await
            .map(|ok| WriteFinishOk { written: ok.written, digest: ok.digest })
            .map_err(|error| match error {
                ota::WriteFinishError::NotWriting => WriteFinishError::NotWriting,
                ota::WriteFinishError::DigestMismatch => WriteFinishError::DigestMismatch,
                ota::WriteFinishError::Incomplete => WriteFinishError::Incomplete,
                ota::WriteFinishError::Storage(_) => WriteFinishError::Storage,
            })
    }
}
