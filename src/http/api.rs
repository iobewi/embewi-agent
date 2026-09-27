//! Contrat v1alpha1 JSON API (`/v1alpha1/*`) -- not the provisioning UI
//! (that's [`super::config`]). `http::run` calls [`serve`] instead of
//! [`super::config::serve`] once the device is locked (i.e. on every boot
//! after the first successful provisioning); see `http/mod.rs`'s module
//! doc for why they're two plain functions sharing one task's `Future`
//! storage, not two separate `#[embassy_executor::task]`s.

use alloc::format;
use alloc::string::String;

use embassy_executor::Spawner;
use embassy_net::Stack;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use esp_hal::peripherals::LPWR;
use picoserve::response::StatusCode;
use picoserve::routing::{get, post, put_service};
use static_cell::StaticCell;

use crate::agent;
use crate::ota;
use espbewi_config_space::NvsConfigBackend;
use espbewi_flash::SharedFlash;

use super::{json_error, json_ok, reboot_after_delay, unauthorized};

mod ota_write;
use ota_write::OtaWrite;

/// Plain `async fn`, not `#[embassy_executor::task]`: called from inside
/// `http::run`'s own `if is_locked() {...} else {...}` (see that module's
/// doc comment) rather than spawned as an independent task, so its
/// `Future`'s storage shares space with [`super::config::serve`]'s instead
/// of both being reserved simultaneously and permanently.
pub async fn serve(
    stack: Stack<'static>,
    flash: &'static SharedFlash,
    nvs_backend: &'static NvsConfigBackend,
    agent_config: &'static agent::AgentConfigSpace,
    app_config: &'static crate::app_config::AppConfigSpace,
    tls_config: &'static crate::tls::TlsConfigSpace,
    runtime_config: &'static crate::runtime_config::RuntimeConfig,
    ota_config: &'static crate::ota::OtaConfigSpace,
    spawner: Spawner,
    lpwr: LPWR<'static>,
    tls: crate::tls::TlsReferenceStatic,
) -> ! {
    // `lpwr` (a non-`Copy` owned peripheral) must move into
    // `reboot_after_delay` exactly once, but both `/reboot` and
    // `/ota/activate` can trigger it -- parking it behind a lock either can
    // `take()` from is simpler than trying to move it into two closures.
    static LPWR_CELL: StaticCell<Mutex<CriticalSectionRawMutex, Option<LPWR<'static>>>> =
        StaticCell::new();
    let lpwr_cell = &*LPWR_CELL.init(Mutex::new(Some(lpwr)));

    let router = picoserve::Router::new()
        .route(
            "/v1alpha1/info",
            get(move |agent::Bearer(token): agent::Bearer| async move {
                if !agent::is_authorized(agent_config, token.as_deref().unwrap_or("")).await {
                    return unauthorized();
                }
                json_ok(serde_json::to_string(&agent::info(flash, agent_config, app_config, runtime_config, ota_config).await).unwrap_or_default())
            }),
        )
        .route(
            "/v1alpha1/health",
            get(move |agent::Bearer(token): agent::Bearer| async move {
                if !agent::is_authorized(agent_config, token.as_deref().unwrap_or("")).await {
                    return unauthorized();
                }
                json_ok(serde_json::to_string(&agent::health(nvs_backend).await).unwrap_or_default())
            }),
        )
        .route(
            "/v1alpha1/config",
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
            "/v1alpha1/token",
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
            "/v1alpha1/reboot",
            post(move |agent::Bearer(token): agent::Bearer| async move {
                if !agent::is_authorized(agent_config, token.as_deref().unwrap_or("")).await {
                    return unauthorized();
                }
                // Same one-shot `lpwr_cell` as `/ota/activate` below --
                // whichever fires first gets to actually reboot the
                // device; there's only one `lpwr` to hand out either way.
                if let Some(lpwr) = lpwr_cell.lock().await.take()
                    && let Ok(spawn_token) = reboot_after_delay(lpwr)
                {
                    spawner.spawn(spawn_token);
                }
                json_ok(String::from("{\"status\":\"rebooting\"}"))
            }),
        )
        .route(
            "/v1alpha1/app/port",
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
        // OTA A/B (contrat §3/§4/§6) -- streaming write logic lives in
        // `ota_write.rs`, everything else (`src/ota.rs`) is just called
        // through like the routes above.
        .route(
            "/v1alpha1/ota/prepare",
            post(move |agent::Bearer(token): agent::Bearer, body: String| async move {
                if !agent::is_authorized(agent_config, token.as_deref().unwrap_or("")).await {
                    return unauthorized();
                }
                let Ok(req) = serde_json::from_str::<ota::PrepareRequest>(&body) else {
                    return json_error(StatusCode::BAD_REQUEST, "{\"error\":\"bad_request\"}");
                };
                let resp = ota::prepare(flash, ota_config, &req).await;
                json_ok(serde_json::to_string(&resp).unwrap_or_default())
            }),
        )
        .route("/v1alpha1/ota/write", put_service(OtaWrite { flash, ota_config, agent_config }))
        .route(
            "/v1alpha1/ota/activate",
            post(move |agent::Bearer(token): agent::Bearer, body: String| async move {
                if !agent::is_authorized(agent_config, token.as_deref().unwrap_or("")).await {
                    return unauthorized();
                }
                #[derive(serde::Deserialize)]
                struct ActivateBody {
                    deployment_id: String,
                }
                let Ok(req) = serde_json::from_str::<ActivateBody>(&body) else {
                    return json_error(StatusCode::BAD_REQUEST, "{\"error\":\"missing_deployment_id\"}");
                };
                let target_slot = match ota::activate(flash, ota_config, &req.deployment_id).await {
                    Ok(slot) => slot,
                    Err(ota::ActivateError::DeploymentMismatch) => {
                        return json_error(StatusCode::CONFLICT, "{\"error\":\"deployment_mismatch\"}");
                    }
                    Err(ota::ActivateError::NotStaged) => {
                        return json_error(StatusCode::CONFLICT, "{\"error\":\"not_staged\"}");
                    }
                    Err(ota::ActivateError::Storage(_)) => {
                        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "{\"error\":\"nvs_write_failed\"}");
                    }
                };
                // Same one-shot `lpwr_cell` as `/reboot` above -- there's
                // only one `lpwr` to hand out, whichever fires first wins.
                if let Some(lpwr) = lpwr_cell.lock().await.take()
                    && let Ok(spawn_token) = reboot_after_delay(lpwr)
                {
                    spawner.spawn(spawn_token);
                }
                json_ok(format!("{{\"status\":\"rebooting\",\"target_slot\":\"{target_slot}\"}}"))
            }),
        )
        .route(
            "/v1alpha1/tls/cert",
            post(move |agent::Bearer(token): agent::Bearer, body: String| async move {
                if !agent::is_authorized(agent_config, token.as_deref().unwrap_or("")).await {
                    return unauthorized();
                }
                #[derive(serde::Deserialize)]
                struct CertBody {
                    cert_pem: String,
                    key_pem: String,
                }
                let Ok(req) = serde_json::from_str::<CertBody>(&body) else {
                    return json_error(StatusCode::BAD_REQUEST, "{\"error\":\"missing_cert_or_key\"}");
                };
                match crate::tls::save_cert(tls_config, &req.cert_pem, &req.key_pem).await {
                    Ok(()) => json_ok(String::from("{\"status\":\"saved\"}")),
                    Err(crate::tls::SaveCertError::Invalid) => {
                        json_error(StatusCode::BAD_REQUEST, "{\"error\":\"invalid_certificate\"}")
                    }
                    Err(crate::tls::SaveCertError::Mismatch) => {
                        json_error(StatusCode::BAD_REQUEST, "{\"error\":\"cert_key_mismatch\"}")
                    }
                    Err(crate::tls::SaveCertError::Storage) => {
                        json_error(StatusCode::INTERNAL_SERVER_ERROR, "{\"error\":\"nvs_write_failed\"}")
                    }
                }
            }),
        )
        .route(
            "/v1alpha1/tls/ca",
            post(move |agent::Bearer(token): agent::Bearer, body: String| async move {
                if !agent::is_authorized(agent_config, token.as_deref().unwrap_or("")).await {
                    return unauthorized();
                }
                #[derive(serde::Deserialize)]
                struct CaBody {
                    ca_pem: String,
                }
                let Ok(req) = serde_json::from_str::<CaBody>(&body) else {
                    return json_error(StatusCode::BAD_REQUEST, "{\"error\":\"missing_ca\"}");
                };
                match crate::tls::save_ca(tls_config, &req.ca_pem).await {
                    Ok(()) => json_ok(String::from("{\"status\":\"saved\"}")),
                    Err(crate::tls::SaveCertError::Storage) => {
                        json_error(StatusCode::INTERNAL_SERVER_ERROR, "{\"error\":\"nvs_write_failed\"}")
                    }
                    Err(_) => json_error(StatusCode::BAD_REQUEST, "{\"error\":\"invalid_certificate\"}"),
                }
            }),
        );

    super::serve(stack, tls_config, tls, &router).await
}
