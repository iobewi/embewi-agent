//! HTTPS-only administrative surfaces.
//!
//! embewi-agent and embewi-init deliberately expose different routers:
//! - the runtime agent exposes only the v1alpha1 API;
//! - the disposable init image exposes only the provisioning UI.
//!
//! They share the TLS accept loop below but never dispatch between surfaces
//! at runtime. Port 80 is never bound and there is no clear-text fallback.
//! The application-service TCP port reported by /info remains a separate
//! business-plane setting and is unrelated to this fixed admin HTTPS port.

use alloc::string::String;

use embassy_executor::Spawner;
use embassy_net::Stack;
use embassy_time::{Duration, Timer};
use esp_hal::peripherals::LPWR;
use esp_hal::rtc_cntl::{Rtc, RwdtStage, RwdtStageAction};
use picoserve::response::{ContentBody, ContentHeaders, Response, StatusCode};
use picoserve::routing::PathRouter;

use iobewi_esp_config_space::NvsConfigBackend;
use iobewi_esp_flash::SharedFlash;

pub mod api;
pub mod config;

/// Runtime administrative API task. Provisioning has a separate entrypoint
/// and is linked only by the disposable init image.
#[embassy_executor::task]
pub async fn run(
    stack: Stack<'static>,
    flash: &'static SharedFlash,
    nvs_backend: &'static NvsConfigBackend,
    agent_config: &'static crate::agent::AgentConfigSpace,
    app_config: &'static crate::app_config::AppConfigSpace,
    tls_config: &'static crate::tls::TlsConfigSpace,
    runtime_config: &'static crate::runtime_config::RuntimeConfig,
    ota_config: &'static crate::ota::OtaConfigSpace,
    spawner: Spawner,
    lpwr: LPWR<'static>,
    tls: crate::tls::TlsReferenceStatic,
) -> ! {
    api::serve(
        stack,
        flash,
        nvs_backend,
        agent_config,
        app_config,
        tls_config,
        runtime_config,
        ota_config,
        spawner,
        lpwr,
        tls,
    )
    .await
}

/// HTTPS provisioning surface used only by embewi-init.
#[embassy_executor::task]
pub async fn run_provisioning(
    stack: Stack<'static>,
    flash: &'static SharedFlash,
    agent_config: &'static crate::agent::AgentConfigSpace,
    hardware_config: &'static crate::hardware::HardwareConfigSpace,
    tls_config: &'static crate::tls::TlsConfigSpace,
    lifecycle_config: &'static crate::lifecycle::LifecycleConfigSpace,
    ota_config: &'static crate::ota::OtaConfigSpace,
    factory_agent: crate::ota::PreloadedAgent,
    spawner: Spawner,
    lpwr: LPWR<'static>,
    tls: crate::tls::TlsReferenceStatic,
) -> ! {
    config::serve(
        stack,
        flash,
        agent_config,
        hardware_config,
        tls_config,
        lifecycle_config,
        ota_config,
        factory_agent,
        spawner,
        lpwr,
        tls,
    )
    .await
}

// Shared with web/index.html (the flashing page), so both look consistent
// -- one canonical file instead of a copy that could drift.
const STYLE_CSS: &str = include_str!("../../web/style.css");

/// Escapes `&`/`<`/`>`/`"` so admin-supplied text reflected back into
/// `value="..."` attributes can't break out of the attribute or inject
/// markup.
pub(super) fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Named explicitly (not `impl IntoResponse`): every `/v1alpha1/*` handler
/// branches between this and [`json_error`]/[`unauthorized`], and separate
/// `impl Trait` return sites never unify even when the concrete type
/// matches -- picoserve's own `Response::ok`/`::new` already resolve to
/// this same `Response<ContentHeaders, ContentBody<String>>` either way.
pub(super) type JsonResponse = Response<ContentHeaders, ContentBody<String>>;

pub(super) fn json_ok(body: String) -> JsonResponse {
    Response::ok(body).with_content_type("application/json")
}

pub(super) fn json_error(status: StatusCode, body: &str) -> JsonResponse {
    Response::new(status, String::from(body)).with_content_type("application/json")
}

/// Shared by every `/v1alpha1/*` handler (contrat §4b: `401
/// {"error":"unauthorized"}`).
pub(super) fn unauthorized() -> JsonResponse {
    json_error(StatusCode::UNAUTHORIZED, "{\"error\":\"unauthorized\"}")
}

/// Gives the response time to actually reach the socket before resetting --
/// calling a reset directly from the request handler would cut the
/// connection before picoserve ever writes the confirmation page.
///
/// Uses the RTC watchdog (`ResetSystem`, the broadest of the three reset
/// scopes esp-hal exposes) instead of `esp_hal::system::software_reset()`.
/// That function only does a "digital core" reset, which on this chip
/// leaves the native USB-Serial-JTAG peripheral's link state untouched: the
/// host still sees the old USB session, the freshly-booted firmware expects
/// a new one, and Improv Serial stops responding correctly until a real
/// (EN-pin/RTS-triggered) reset -- exactly what ESP Web Tools itself always
/// does when it resets the board, which is why that path never showed this.
#[embassy_executor::task]
pub(super) async fn reboot_after_delay(lpwr: LPWR<'static>) -> ! {
    Timer::after(Duration::from_millis(500)).await;
    let mut rtc = Rtc::new(lpwr);
    rtc.rwdt
        .set_timeout(RwdtStage::Stage0, esp_hal::time::Duration::from_millis(100));
    rtc.rwdt.set_stage_action(RwdtStage::Stage0, RwdtStageAction::ResetSystem);
    rtc.rwdt.enable();
    loop {
        Timer::after(Duration::from_secs(10)).await;
    }
}

/// The runtime and provisioning routers share the IOBEWI HTTP server.
/// The ESP listener supplies authenticated TLS sockets; the application
/// supplies only its identity store and routes.
pub(super) async fn serve(
    stack: Stack<'static>,
    tls_config: &'static crate::tls::TlsConfigSpace,
    tls: crate::tls::TlsReferenceStatic,
    router: &picoserve::Router<impl PathRouter>,
) -> ! {
    iobewi_esp_https::serve(
        stack,
        tls,
        || crate::tls::server_config(tls_config),
        router,
    ).await
}
