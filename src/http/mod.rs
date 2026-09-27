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
use embassy_net::tcp::TcpSocket;
use embassy_time::{Duration, Timer, with_timeout};
use esp_hal::peripherals::LPWR;
use esp_hal::rtc_cntl::{Rtc, RwdtStage, RwdtStageAction};
use espbewi_tls::mbedtls_rs::SessionError;
use log::{debug, warn};
use picoserve::io::Socket;
use picoserve::response::{ContentBody, ContentHeaders, Response, StatusCode};
use picoserve::routing::PathRouter;

use config_space_manager_esp_nvs::NvsConfigBackend;
use espbewi_flash::SharedFlash;

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

/// Fixed HTTPS port for every Embewi administrative surface.
/// Port 80 is intentionally never bound.
const ADMIN_PORT_HTTPS: u16 = 443;

/// Bound on the TLS handshake specifically, well short of the 45 s socket
/// idle timeout below (which covers the *whole* connection, handshake
/// included, but is meant to tolerate a slow legitimate client mid-request,
/// not a stalled/adversarial handshake). This server handles one connection
/// at a time (this module's own doc comment): without this, a client that
/// opens the TCP connection and then stalls the handshake (a partial/absent
/// `ClientHello`, or bytes dripped just fast enough to keep resetting the
/// socket's own idle timer) can hold the only admin connection for up to
/// that 45 s on every single attempt, locking out legitimate Core traffic.
/// 5 s is generous for a real ECDHE handshake on this CPU (~0.5-1 s
/// observed) with LAN-grade margin.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Drives the accept loop for one router (`config` or `api`) to completion
/// -- generic over [`PathRouter`] rather than duplicated per caller, but
/// still monomorphized (and so given its own, separately-sized `Future`)
/// once per concrete router type, which is the whole point of splitting
/// `config`/`api` into two tasks in the first place (see this module's doc
/// comment).
pub(super) async fn serve(
    stack: Stack<'static>,
    tls_config: &'static crate::tls::TlsConfigSpace,
    tls: crate::tls::TlsReferenceStatic,
    router: &picoserve::Router<impl PathRouter>,
) -> ! {
    let config = picoserve::Config::const_default().keep_connection_alive();
    let mut rx_buffer = [0u8; 1024];
    let mut tx_buffer = [0u8; 1024];
    let mut http_buffer = [0u8; 2048];

    // Accepts the raw TCP connection ourselves instead of calling picoserve's
    // `listen_and_serve` (which is hardcoded to `embassy_net::tcp::TcpSocket`
    // internally). `serve_connection` below only needs a
    // `picoserve::io::Socket`, so plugging in TLS never touches the router
    // or handlers.
    loop {
        let Some(tls_server_config) = crate::tls::server_config(tls_config).await else {
            // Security invariant: absence/corruption of the server identity
            // never downgrades the device to HTTP.
            warn!("HTTPS: no usable server identity; administrative surface remains closed");
            Timer::after(Duration::from_secs(5)).await;
            continue;
        };

        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        if let Err(e) = socket.accept(ADMIN_PORT_HTTPS).await {
            warn!("HTTPS: accept failed: {e:?}");
            continue;
        }
        socket.set_keep_alive(Some(Duration::from_secs(30)));
        socket.set_timeout(Some(Duration::from_secs(45)));

        let mut session =
            match espbewi_tls::mbedtls_rs::Session::new(tls, socket, &tls_server_config) {
                Ok(session) => session,
                Err(e) => {
                    warn!("HTTPS: session setup failed: {e}");
                    continue;
                }
            };
        match with_timeout(HANDSHAKE_TIMEOUT, session.connect()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                if is_peer_hangup(&e) {
                    // Expected: e.g. a client's connection aborting mid-handshake
                    // right as `reboot_after_delay`'s hardware reset fires (or any
                    // client that simply drops the connection). Not a server
                    // fault, so it stays out of `warn!`.
                    debug!("HTTPS: handshake aborted by peer: {e}");
                } else {
                    warn!("HTTPS: handshake failed: {e}");
                }
                continue;
            }
            Err(_) => {
                warn!("HTTPS: handshake timed out after {HANDSHAKE_TIMEOUT:?}");
                continue;
            }
        }
        if let Err(e) = serve_connection(
            router,
            &config,
            &mut http_buffer,
            crate::tls::TlsSocket::new(session),
        )
        .await
        {
            let hangup = match &e {
                picoserve::Error::Read(inner) | picoserve::Error::Write(inner) => is_peer_hangup(inner),
                _ => false,
            };
            if hangup {
                debug!("HTTPS: connection closed by peer: {e:?}");
            } else {
                warn!("HTTPS: connection error: {e:?}");
            }
        }
    }
}

/// Whether `e` is the peer's TLS stack (or its abrupt disconnect, reported
/// this way by mbedtls) sending a fatal alert -- a normal connection
/// lifecycle event (e.g. a client dropping mid-handshake or mid-request
/// right as `reboot_after_delay`'s hardware reset fires), not a defect on
/// our side.
fn is_peer_hangup(e: &SessionError) -> bool {
    // MBEDTLS_ERR_SSL_FATAL_ALERT_MESSAGE (mbedtls's `ssl.h`).
    const FATAL_ALERT_MESSAGE: i32 = -0x7780;
    matches!(e, SessionError::MbedTls(m) if m.code() == FATAL_ALERT_MESSAGE)
}

/// Serves one already-connected socket to completion. Generic over
/// [`picoserve::io::Socket`] rather than a concrete transport, so plugging in
/// TLS is a matter of handing this a TLS-wrapped socket instead of a bare
/// [`TcpSocket`] -- the router and every handler are unaware of the
/// transport either way.
async fn serve_connection<S: Socket<picoserve::EmbassyRuntime>>(
    router: &picoserve::Router<impl PathRouter>,
    config: &picoserve::Config,
    http_buffer: &mut [u8],
    socket: S,
) -> Result<picoserve::DisconnectionInfo<picoserve::NoGracefulShutdown>, picoserve::Error<S::Error>>
{
    picoserve::Server::new(router, config, http_buffer)
        .serve(socket)
        .await
}
