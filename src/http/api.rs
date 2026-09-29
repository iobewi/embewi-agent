//! Contrat v1alpha1 JSON API (`/v1alpha1/*`) -- not the provisioning UI
//! (that's [`super::config`]). `http::run` calls [`serve`] instead of
//! [`super::config::serve`] once the device is locked (i.e. on every boot
//! after the first successful provisioning); see `http/mod.rs`'s module
//! doc for why they're two plain functions sharing one task's `Future`
//! storage, not two separate `#[embassy_executor::task]`s.

use alloc::format;
use alloc::string::String;

use picoserve::response::StatusCode;
use iobewi_http::routing::{get, post};
use iobewi_http::HttpRouter;
use iobewi_ota::http::RebootPort;

use crate::agent;
use iobewi_config_space::ConfigSpace;
use crate::ota;
use iobewi_esp_config_space::NvsConfigBackend;
// SharedFlash is still needed here only to construct `AgentOtaBackend`
// below (the OTA backend that legitimately owns the flash handle); it is
// no longer used for `/info`, which now goes through `BootInfoSource`.
use iobewi_esp_flash::SharedFlash;

use super::{json_error, json_ok, unauthorized};

mod ota_write;
use ota_write::AgentOtaBackend;

/// Public API namespace selected by EmBewi, independent of service routes.
pub(crate) const API_PREFIX: &str = "/v1alpha1";

/// Plain `async fn`, not `#[embassy_executor::task]`: called from inside
/// `http::run`'s own `if is_locked() {...} else {...}` (see that module's
/// doc comment) rather than spawned as an independent task, so its
/// `Future`'s storage shares space with [`super::config::serve`]'s instead
/// of both being reserved simultaneously and permanently.
///
/// `tls_backend` supplies TLS-identity authorization/persistence through the
/// portable `iobewi_tls::http::ProvisioningBackend` capability -- this
/// module never knows how certificates are validated or stored, only that
/// `cert_response`/`ca_response` need a backend to call.
pub async fn serve<L, R, TB, H, B>(
    listener: &mut L,
    flash: &'static SharedFlash,
    storage: &'static H,
    agent_config: &'static agent::AgentConfigSpace,
    app_config: &'static ConfigSpace<NvsConfigBackend>,
    runtime_config: &'static crate::runtime_config::RuntimeConfig,
    ota_config: &'static crate::ota::OtaConfigSpace,
    reboot: R,
    tls_backend: TB,
    boot: &'static B,
) -> !
where
    L: iobewi_https::TlsListener,
    R: RebootPort + Clone + 'static,
    TB: iobewi_tls::http::ProvisioningBackend + Clone + 'static,
    H: agent::StorageHealth,
    B: agent::BootInfoSource,
{
    let api_routes = HttpRouter::new()
        .route(
            "/info",
            get(move |agent::Bearer(token): agent::Bearer| async move {
                if !agent::is_authorized(agent_config, token.as_deref().unwrap_or("")).await {
                    return unauthorized();
                }
                json_ok(serde_json::to_string(&agent::info(boot, agent_config, app_config, runtime_config, ota_config).await).unwrap_or_default())
            }),
        )
        .route(
            "/health",
            get(move |agent::Bearer(token): agent::Bearer| async move {
                if !agent::is_authorized(agent_config, token.as_deref().unwrap_or("")).await {
                    return unauthorized();
                }
                json_ok(serde_json::to_string(&agent::health(storage).await).unwrap_or_default())
            }),
        )
        .route(
            "/config",
            get(move |agent::Bearer(token): agent::Bearer| async move {
                if !agent::is_authorized(agent_config, token.as_deref().unwrap_or("")).await {
                    return unauthorized();
                }
                match runtime_config.view().await {
                    Ok(view) => json_ok(serde_json::to_string(&view).unwrap_or_default()),
                    Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "{\"error\":\"nvs_read_failed\"}"),
                }
            })
            .post(move |agent::Bearer(token): agent::Bearer, body: String| async move {
                if !agent::is_authorized(agent_config, token.as_deref().unwrap_or("")).await {
                    return unauthorized();
                }
                let Ok(push) = serde_json::from_str::<crate::runtime_config::ConfigPush>(&body) else {
                    return json_error(StatusCode::BAD_REQUEST, "{\"error\":\"missing_data_field\"}");
                };
                let generation = match runtime_config.apply(&push).await {
                    Ok(generation) => generation,
                    Err(_) => {
                        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "{\"error\":\"nvs_write_failed\"}");
                    }
                };
                json_ok(format!(
                    "{{\"status\":\"saved\",\"generation\":{generation},\"note\":\"effective_after_reboot\"}}"
                ))
            }),
        )
        .route(
            "/token",
            post(move |agent::Bearer(token): agent::Bearer, body: String| async move {
                if !agent::is_authorized(agent_config, token.as_deref().unwrap_or("")).await {
                    return unauthorized();
                }
                #[derive(serde::Deserialize)]
                struct TokenBody {
                    token: String,
                }
                let Ok(req) = serde_json::from_str::<TokenBody>(&body) else {
                    return json_error(StatusCode::BAD_REQUEST, "{\"error\":\"missing_token\"}");
                };
                match agent::rotate_token(agent_config, &req.token).await {
                    Ok(()) => json_ok(String::from("{\"status\":\"rotated\"}")),
                    Err(agent::RotateTokenError::InvalidLength) => {
                        json_error(StatusCode::BAD_REQUEST, "{\"error\":\"token must be 8-64 chars\"}")
                    }
                    Err(agent::RotateTokenError::WriteFailed) => json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "{\"error\":\"nvs_write_failed\"}",
                    ),
                }
            }),
        )
        .route(
            "/reboot",
            post({
                // Cloned once here, up front, so the original `reboot` stays
                // available below for `/ota/activate`'s own `.nest(...)`.
                let reboot = reboot.clone();
                move |agent::Bearer(token): agent::Bearer| {
                    // Cloned again per call (this closure must stay `Fn`,
                    // invoked once per request): `reboot` is a one-shot
                    // capability internally, so cloning it freely here and
                    // for `/ota/activate` below is safe -- whichever call
                    // reaches `schedule_reboot()` first is the one that
                    // actually reboots the device.
                    let reboot = reboot.clone();
                    async move {
                        if !agent::is_authorized(agent_config, token.as_deref().unwrap_or("")).await {
                            return unauthorized();
                        }
                        reboot.schedule_reboot().await;
                        json_ok(String::from("{\"status\":\"rebooting\"}"))
                    }
                }
            }),
        )
        .route(
            "/app/port",
            post(move |agent::Bearer(token): agent::Bearer, body: String| async move {
                if !agent::is_authorized(agent_config, token.as_deref().unwrap_or("")).await {
                    return unauthorized();
                }
                #[derive(serde::Deserialize)]
                struct AppPortBody {
                    port: u32,
                }
                let Ok(req) = serde_json::from_str::<AppPortBody>(&body) else {
                    return json_error(StatusCode::BAD_REQUEST, "{\"error\":\"missing_port\"}");
                };
                if !(1024..=65535).contains(&req.port) {
                    return json_error(StatusCode::BAD_REQUEST, "{\"error\":\"port must be 1024-65535\"}");
                }
                if crate::app_config::save_port(app_config, req.port as u16).await.is_err() {
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "{\"error\":\"nvs_write_failed\"}");
                }
                json_ok(format!(
                    "{{\"status\":\"saved\",\"port\":{}}}",
                    req.port
                ))
            }),
        )
        // OTA owns its relative routes. EmBewi supplies only the platform
        // backend and the same reboot capability used by /reboot.
        .nest("/ota", iobewi_ota::http::routes(
            AgentOtaBackend { flash, ota_config, agent_config },
            reboot,
        ))
        // The TLS service owns authentication and the wire contract; the
        // application only mounts its routes on the shared HTTPS server and
        // supplies the injected `tls_backend` capability, never how
        // certificates are validated or persisted.
        .route(
            iobewi_tls::http::CERT_PATH,
            post({
                let tls_backend = tls_backend.clone();
                move |agent::Bearer(token): agent::Bearer, body: String| {
                    let tls_backend = tls_backend.clone();
                    async move {
                        iobewi_tls::http::cert_response(&tls_backend, token.as_deref().unwrap_or(""), &body).await
                    }
                }
            }),
        )
        .route(
            iobewi_tls::http::CA_PATH,
            post({
                let tls_backend = tls_backend.clone();
                move |agent::Bearer(token): agent::Bearer, body: String| {
                    let tls_backend = tls_backend.clone();
                    async move {
                        iobewi_tls::http::ca_response(&tls_backend, token.as_deref().unwrap_or(""), &body).await
                    }
                }
            }),
        );
    let router = HttpRouter::new().nest(API_PREFIX, api_routes);

    super::serve(listener, &router).await
}
