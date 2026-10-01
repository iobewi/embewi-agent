use iobewi_esp_config_space::NvsConfigBackend;

/// Application authorization plus ESP persistence for the portable TLS
/// provisioning API (`iobewi_tls_service::http::ProvisioningBackend`). Built once at
/// composition and injected into `http::api::serve` -- the portable HTTP
/// layer never sees `iobewi_esp_tls` or `TlsConfigSpace` itself.
#[derive(Clone, Copy)]
pub(crate) struct AgentTlsProvisioningBackend {
    pub(crate) tls_config: &'static crate::esp_tls::TlsConfigSpace<NvsConfigBackend>,
    pub(crate) agent_config: &'static crate::agent::AgentConfigSpace<NvsConfigBackend>,
}

impl iobewi_tls_service::http::ProvisioningBackend for AgentTlsProvisioningBackend {
    async fn authorize(&self, token: &str) -> bool {
        crate::agent::is_authorized(self.agent_config, token).await
    }

    async fn save_cert(&self, cert_pem: &str, key_pem: &str) -> Result<(), iobewi_tls_service::SaveCertError> {
        crate::esp_tls::save_cert(self.tls_config, cert_pem, key_pem).await
    }

    async fn save_ca(&self, ca_pem: &str) -> Result<(), iobewi_tls_service::SaveCertError> {
        crate::esp_tls::save_ca(self.tls_config, ca_pem).await
    }
}

