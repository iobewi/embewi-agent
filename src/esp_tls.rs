//! ESP composition of IOBEWI's portable TLS pieces.
//!
//! This is composition glue, not product logic: it fixes the concrete types
//! behind the generic contracts -- MbedTLS crypto over the ESP hardware RNG
//! (`iobewi_crypto_mbedtls::MbedtlsCrypto<EspEntropySource>`) driving the
//! portable `iobewi_tls_service::TlsService`, and the ESP Embassy dialer
//! behind the fail-closed `SecureConnector`. Only composition roots (the
//! binaries and `composition/`) use it; the portable modules never import it.

use embassy_net::Stack;
use iobewi_config_space::{ConfigBackend, ConfigSpace};
use iobewi_crypto_mbedtls::MbedtlsCrypto;
use iobewi_esp_entropy::EspEntropySource;
use iobewi_esp_tls::embassy::EspTlsDialer;
use iobewi_tls_service::client::SecureConnector;
use iobewi_tls_service::TlsService;
use iobewi_esp_tls::mbedtls_rs::SessionConfig;

pub use iobewi_esp_tls::TlsReferenceStatic;
pub use iobewi_tls_service::{CONFIG_BUDGET, IdentityBootstrapError, SaveCertError};

pub type TlsConfigSpace<B> = ConfigSpace<B>;

/// The ESP implementation of the secure outbound connector
/// (`SecureClientTransport`): fail-closed on clock and CA, ESP DNS/TCP/TLS.
pub type EspClientTransport<B> = SecureConnector<EspTlsDialer, B>;

fn service() -> TlsService<MbedtlsCrypto<EspEntropySource>> {
    TlsService::new(MbedtlsCrypto::new(EspEntropySource))
}

/// Installs the MbedTLS hooks and creates the process-global TLS instance.
/// Call exactly once; `now` is the wall clock (`None` until synchronized).
pub fn init(now: iobewi_esp_tls::UnixTimeFn) -> TlsReferenceStatic {
    iobewi_esp_tls::init(now)
}

pub fn client_transport<B: ConfigBackend + 'static>(
    tls: TlsReferenceStatic,
    stack: Stack<'static>,
    tls_config: &'static TlsConfigSpace<B>,
    clock_is_set: fn() -> bool,
) -> EspClientTransport<B> {
    SecureConnector { dialer: EspTlsDialer { tls, stack }, tls_config, clock_is_set }
}

pub async fn ensure_server_identity<B: ConfigBackend>(space: &TlsConfigSpace<B>, common_name: &str) -> Result<(), IdentityBootstrapError>
where B::Error: core::fmt::Debug {
    service().ensure_server_identity(space, common_name).await
}

pub async fn server_identity_valid<B: ConfigBackend>(space: &TlsConfigSpace<B>) -> bool
where B::Error: core::fmt::Debug {
    service().server_identity_valid(space).await
}

pub async fn save_cert<B: ConfigBackend>(space: &TlsConfigSpace<B>, cert_pem: &str, key_pem: &str) -> Result<(), SaveCertError>
where B::Error: core::fmt::Debug {
    service().save_cert(space, cert_pem, key_pem).await
}

pub async fn server_config<B: ConfigBackend>(space: &TlsConfigSpace<B>) -> Option<SessionConfig<'static>>
where B::Error: core::fmt::Debug {
    service().server_config(space).await
}

pub async fn save_ca<B: ConfigBackend>(space: &TlsConfigSpace<B>, ca_pem: &str) -> Result<(), SaveCertError>
where B::Error: core::fmt::Debug {
    service().save_ca(space, ca_pem).await
}
