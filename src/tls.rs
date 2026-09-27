//! Embewi TLS policy and persistent configuration.
//!
//! Reusable ESP/MbedTLS mechanics live in `espbewi-tls`. This module
//! owns only the Embewi TLS configuration schema and policy. Persistence is
//! one opaque ConfigSpace value: CA + server certificate + private key are
//! replaced atomically by config-space-manager's backend.

use alloc::string::String;
use alloc::vec::Vec;

use config_space_manager::{Budget, ConfigSpace};
use embassy_net::tcp::TcpSocket;
use espbewi_tls::mbedtls_rs::{Session, SessionConfig, SessionError};
use log::warn;

use config_space_manager_esp_nvs::NvsConfigBackend;

const MAGIC: &[u8; 4] = b"TLS1";
const HEADER_LEN: usize = 10;
const MAX_CA_LEN: usize = 2048;
const MAX_CERT_LEN: usize = 2048;
const MAX_KEY_LEN: usize = 2048;

/// Header plus the maximum encoded CA/certificate/private-key payload.
pub const CONFIG_BUDGET: Budget =
    Budget::new(HEADER_LEN + MAX_CA_LEN + MAX_CERT_LEN + MAX_KEY_LEN);

pub type TlsConfigSpace = ConfigSpace<NvsConfigBackend>;
pub type TlsReferenceStatic = espbewi_tls::TlsReferenceStatic;
pub use espbewi_tls::embassy::PicoserveTlsSocket as TlsSocket;

#[derive(Clone, Default)]
struct TlsConfig {
    ca_pem: String,
    cert_pem: String,
    key_pem: String,
}

impl TlsConfig {
    fn encode(&self) -> Option<Vec<u8>> {
        if self.ca_pem.len() > MAX_CA_LEN
            || self.cert_pem.len() > MAX_CERT_LEN
            || self.key_pem.len() > MAX_KEY_LEN
        {
            return None;
        }

        let ca_len = u16::try_from(self.ca_pem.len()).ok()?;
        let cert_len = u16::try_from(self.cert_pem.len()).ok()?;
        let key_len = u16::try_from(self.key_pem.len()).ok()?;
        let total = HEADER_LEN
            .checked_add(self.ca_pem.len())?
            .checked_add(self.cert_pem.len())?
            .checked_add(self.key_pem.len())?;
        if total > CONFIG_BUDGET.max_bytes() {
            return None;
        }

        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&ca_len.to_le_bytes());
        out.extend_from_slice(&cert_len.to_le_bytes());
        out.extend_from_slice(&key_len.to_le_bytes());
        out.extend_from_slice(self.ca_pem.as_bytes());
        out.extend_from_slice(self.cert_pem.as_bytes());
        out.extend_from_slice(self.key_pem.as_bytes());
        Some(out)
    }

    fn decode(raw: &[u8]) -> Option<Self> {
        if raw.len() < HEADER_LEN || &raw[..4] != MAGIC {
            return None;
        }
        let ca_len = u16::from_le_bytes([raw[4], raw[5]]) as usize;
        let cert_len = u16::from_le_bytes([raw[6], raw[7]]) as usize;
        let key_len = u16::from_le_bytes([raw[8], raw[9]]) as usize;
        if ca_len > MAX_CA_LEN || cert_len > MAX_CERT_LEN || key_len > MAX_KEY_LEN {
            return None;
        }

        let ca_end = HEADER_LEN.checked_add(ca_len)?;
        let cert_end = ca_end.checked_add(cert_len)?;
        let key_end = cert_end.checked_add(key_len)?;
        if key_end != raw.len() {
            return None;
        }

        Some(Self {
            ca_pem: String::from(core::str::from_utf8(&raw[HEADER_LEN..ca_end]).ok()?),
            cert_pem: String::from(core::str::from_utf8(&raw[ca_end..cert_end]).ok()?),
            key_pem: String::from(core::str::from_utf8(&raw[cert_end..key_end]).ok()?),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadError {
    Storage,
    Corrupt,
}

async fn load_existing(space: &TlsConfigSpace) -> Result<Option<TlsConfig>, LoadError> {
    match space.load().await {
        Ok(Some(snapshot)) => TlsConfig::decode(&snapshot.data)
            .map(Some)
            .ok_or_else(|| {
                warn!(
                    "tls: stored config generation={} has an unsupported/corrupt schema",
                    snapshot.generation
                );
                LoadError::Corrupt
            }),
        Ok(None) => Ok(None),
        Err(e) => {
            warn!("tls: config-space load failed: {e:?}");
            Err(LoadError::Storage)
        }
    }
}

async fn load_config(space: &TlsConfigSpace) -> Option<TlsConfig> {
    match load_existing(space).await {
        Ok(Some(config)) => Some(config),
        Ok(None) => Some(TlsConfig::default()),
        Err(_) => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityBootstrapError {
    Storage,
    Corrupt,
    Generation,
    Invalid,
}

/// Ensures that a server identity exists before any provisioning network
/// service is started.
///
/// Absence is the only state in which a new identity is generated. A stored
/// but corrupt/partial identity fails closed: storage damage must never
/// silently change the device identity.
pub async fn ensure_server_identity(
    space: &TlsConfigSpace,
    common_name: &str,
) -> Result<(), IdentityBootstrapError> {
    match load_existing(space).await {
        Ok(Some(config)) => {
            if config.cert_pem.is_empty() || config.key_pem.is_empty() {
                return Err(IdentityBootstrapError::Corrupt);
            }
            if espbewi_tls::validate_cert_key_pair(&config.cert_pem, &config.key_pem).is_err()
                || espbewi_tls::server_config_from_pem(&config.cert_pem, &config.key_pem)
                    .is_err()
            {
                return Err(IdentityBootstrapError::Invalid);
            }
            return Ok(());
        }
        Ok(None) => {}
        Err(LoadError::Storage) => return Err(IdentityBootstrapError::Storage),
        Err(LoadError::Corrupt) => return Err(IdentityBootstrapError::Corrupt),
    }

    let generated = espbewi_tls::generate_self_signed_identity(common_name).map_err(|e| {
        // The variant only says "generation"; the MbedTLS/PSA status is what
        // tells a missing RNG from a certificate-encoding failure.
        warn!("tls: identity generation failed: {e:?}");
        IdentityBootstrapError::Generation
    })?;
    if generated.cert_pem.len() > MAX_CERT_LEN || generated.key_pem.len() > MAX_KEY_LEN {
        return Err(IdentityBootstrapError::Generation);
    }
    if espbewi_tls::validate_cert_key_pair(&generated.cert_pem, &generated.key_pem).is_err()
        || espbewi_tls::server_config_from_pem(&generated.cert_pem, &generated.key_pem).is_err()
    {
        return Err(IdentityBootstrapError::Invalid);
    }

    let config = TlsConfig {
        ca_pem: String::new(),
        cert_pem: generated.cert_pem,
        key_pem: generated.key_pem,
    };
    let encoded = config.encode().ok_or(IdentityBootstrapError::Storage)?;
    space
        .commit(&encoded)
        .await
        .map_err(|_| IdentityBootstrapError::Storage)?;

    // Commit success is not enough for an identity root: re-read and parse the
    // durable bytes before allowing provisioning to reach the network.
    match load_existing(space).await {
        Ok(Some(stored))
            if !stored.cert_pem.is_empty()
                && !stored.key_pem.is_empty()
                && espbewi_tls::validate_cert_key_pair(
                    &stored.cert_pem,
                    &stored.key_pem,
                )
                .is_ok()
                && espbewi_tls::server_config_from_pem(
                    &stored.cert_pem,
                    &stored.key_pem,
                )
                .is_ok() =>
        {
            Ok(())
        }
        Ok(_) | Err(LoadError::Corrupt) => Err(IdentityBootstrapError::Corrupt),
        Err(LoadError::Storage) => Err(IdentityBootstrapError::Storage),
    }
}

/// Runtime prerequisite check used by embewi-agent. It never generates or
/// repairs an identity.
pub async fn server_identity_valid(space: &TlsConfigSpace) -> bool {
    match load_existing(space).await {
        Ok(Some(config)) if !config.cert_pem.is_empty() && !config.key_pem.is_empty() => {
            espbewi_tls::validate_cert_key_pair(&config.cert_pem, &config.key_pem).is_ok()
                && espbewi_tls::server_config_from_pem(&config.cert_pem, &config.key_pem)
                    .is_ok()
        }
        _ => false,
    }
}

/// Initializes the global MbedTLS instance using Embewi's SNTP-derived wall
/// clock. Before SNTP converges, the callback returns `None`, so certificate
/// date validation fails closed.
pub fn init() -> TlsReferenceStatic {
    espbewi_tls::init(crate::time::now)
}

pub enum SaveCertError {
    Invalid,
    Mismatch,
    Storage,
}

/// Validates and atomically replaces the server certificate/private-key pair.
/// The current CA is preserved in the same ConfigSpace value.
pub async fn save_cert(
    space: &TlsConfigSpace,
    cert_pem: &str,
    key_pem: &str,
) -> Result<(), SaveCertError> {
    match espbewi_tls::validate_cert_key_pair(cert_pem, key_pem) {
        Ok(()) => {}
        Err(espbewi_tls::PairError::Mismatch) => return Err(SaveCertError::Mismatch),
        Err(
            espbewi_tls::PairError::InvalidCertificate
            | espbewi_tls::PairError::InvalidPrivateKey,
        ) => return Err(SaveCertError::Invalid),
    }

    if espbewi_tls::server_config_from_pem(cert_pem, key_pem).is_err() {
        return Err(SaveCertError::Invalid);
    }

    let mut config = load_config(space).await.ok_or(SaveCertError::Storage)?;
    config.cert_pem = String::from(cert_pem);
    config.key_pem = String::from(key_pem);
    let encoded = config.encode().ok_or(SaveCertError::Storage)?;
    space
        .commit(&encoded)
        .await
        .map_err(|_| SaveCertError::Storage)?;
    Ok(())
}

/// Builds the current HTTPS server configuration. Missing or invalid
/// identity means there is no administrative listener; there is never an
/// HTTP fallback.
pub async fn server_config(space: &TlsConfigSpace) -> Option<SessionConfig<'static>> {
    let config = load_config(space).await?;
    if config.cert_pem.is_empty() || config.key_pem.is_empty() {
        return None;
    }
    espbewi_tls::server_config_from_pem(&config.cert_pem, &config.key_pem).ok()
}

/// Validates and atomically replaces the CA trusted by outbound TLS
/// connections while preserving the current server certificate/key.
pub async fn save_ca(space: &TlsConfigSpace, ca_pem: &str) -> Result<(), SaveCertError> {
    if !espbewi_tls::validate_ca_pem(ca_pem) {
        return Err(SaveCertError::Invalid);
    }

    let mut config = load_config(space).await.ok_or(SaveCertError::Storage)?;
    config.ca_pem = String::from(ca_pem);
    let encoded = config.encode().ok_or(SaveCertError::Storage)?;
    space
        .commit(&encoded)
        .await
        .map_err(|_| SaveCertError::Storage)?;
    Ok(())
}

pub enum ClientTlsError {
    ClockUnsynced,
    NoCa,
    BadCa,
    Dns,
    Tcp(embassy_net::tcp::ConnectError),
    Handshake(SessionError),
}

impl core::fmt::Display for ClientTlsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ClockUnsynced => write!(f, "clock not synchronized yet (SNTP)"),
            Self::NoCa => write!(f, "no CA configured (POST /v1alpha1/tls/ca)"),
            Self::BadCa => write!(f, "stored CA failed to parse"),
            Self::Dns => write!(f, "DNS resolution failed"),
            Self::Tcp(e) => write!(f, "TCP connect failed: {e:?}"),
            Self::Handshake(e) => write!(f, "TLS handshake failed: {e}"),
        }
    }
}

/// Embewi policy wrapper around the reusable Embassy/TLS connector.
pub async fn connect_client<'h, 'buf>(
    tls: TlsReferenceStatic,
    stack: embassy_net::Stack<'static>,
    space: &TlsConfigSpace,
    rx_buffer: &'buf mut [u8],
    tx_buffer: &'buf mut [u8],
    host: &'h core::ffi::CStr,
    port: u16,
) -> Result<Session<'h, TcpSocket<'buf>>, ClientTlsError> {
    if !crate::time::is_set() {
        return Err(ClientTlsError::ClockUnsynced);
    }

    let config = load_config(space).await.ok_or(ClientTlsError::NoCa)?;
    if config.ca_pem.is_empty() {
        return Err(ClientTlsError::NoCa);
    }

    espbewi_tls::embassy::connect_client(
        tls,
        stack,
        rx_buffer,
        tx_buffer,
        host,
        port,
        &config.ca_pem,
    )
    .await
    .map_err(|e| match e {
        espbewi_tls::embassy::ClientConnectError::BadCa => ClientTlsError::BadCa,
        espbewi_tls::embassy::ClientConnectError::Dns => ClientTlsError::Dns,
        espbewi_tls::embassy::ClientConnectError::Tcp(e) => ClientTlsError::Tcp(e),
        espbewi_tls::embassy::ClientConnectError::Handshake(e) => {
            ClientTlsError::Handshake(e)
        }
    })
}
