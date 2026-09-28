//! Heartbeat outbound flow (contrat §5). Device-initiated POST every 5s to
//! `ctrl_url + /v1alpha1/heartbeat`, so it keeps working even for a device
//! that never accepts inbound connections. Silent (skips the tick) while
//! `ctrl_url` is empty -- nothing provisioned yet, nothing to talk to.
//!
//! HTTPS via the platform TLS transport (contrat §5: "le scheme est forcé en
//! https:// indépendamment du scheme stocké dans ctrl_url"). The portable
//! `iobewi-http` client writes requests and drains responses over that stream.
//! Stays silent (no heartbeat sent, tick skipped) until a CA is configured
//! via `POST /v1alpha1/tls/ca` -- see `tls::ClientTlsError::NoCa`.
//!
//! **One TLS connection reused across many heartbeats**, not a fresh
//! handshake every 5s (the previous design: `Connection: close` on every
//! POST). At the old cadence that was 12 handshakes/minute, 720/hour,
//! 17 280/device/day -- pure overhead paid every tick, whether or not
//! anything changed. Structured as an outer reconnect loop around an inner
//! per-heartbeat loop (mirrors `log_stream.rs`'s `run`/`run_session`
//! split): the inner loop keeps sending on the same [`ClientStream`]
//! until something actually requires a new connection -- the socket drops,
//! the server sends `Connection: close`, `ctrl_url` changes, or this
//! response framing cannot be safely reused (see
//! [`iobewi_http::client::drain_response`]) -- at which point it returns and the outer loop
//! reconnects.
//!
//! The one thing that makes reuse safe at all: **fully draining every
//! response**, not just its status line. The old one-shot design could get
//! away with reading a handful of bytes and throwing the rest of the
//! connection away right after (it always closed next). On a kept-open
//! connection, any header or body byte left unread is exactly what the
//! *next* heartbeat's response parser would misread as the start of a new
//! one -- so `iobewi_http::client::drain_response` parses `Content-Length` (or a minimal
//! `Transfer-Encoding: chunked` decoder) and consumes precisely that many
//! body bytes before this loop is allowed to send again. A response with
//! neither framing header present is genuinely ambiguous per HTTP/1.1 (no
//! way to know where its body ends short of reading until the connection
//! closes) -- treated the same as an explicit `Connection: close`, not
//! guessed at.

use alloc::ffi::CString;
use alloc::format;
use alloc::string::String;

use embassy_net::Stack;
use embassy_time::{Duration, Instant, Timer};
use log::{info, warn};
use serde::Serialize;

use crate::agent;
use crate::tls::{ClientStream, TlsReferenceStatic};

const PERIOD: Duration = Duration::from_secs(5);
/// Sentinel meaning "no temperature sensor wired up" (contrat §5) -- always
/// reported for now, this agent doesn't read the SoC's internal sensor yet.
const TEMP_UNAVAILABLE: f32 = -127.0;

#[derive(Serialize)]
struct Heartbeat<'a> {
    node_id: &'a str,
    ip: &'a str,
    ts: u64,
    state: &'static str,
    #[serde(skip_serializing_if = "String::is_empty")]
    deployment_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    firmware_digest: String,
    ota_validated: bool,
    config_generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    uptime_ms: u64,
    heap_free: u32,
    temp_celsius: f32,
    task_hwm_min: u32,
}

/// `ctrl_url` is `scheme://host[:port]` (the scheme itself is ignored --
/// contrat forces HTTPS regardless of what's stored). `None` on a malformed
/// value rather than panicking this task.
fn split_host_port(ctrl_url: &str) -> Option<(&str, u16)> {
    let rest = ctrl_url.split_once("://").map(|(_, rest)| rest).unwrap_or(ctrl_url);
    let host_port = rest.split('/').next().unwrap_or(rest);
    match host_port.split_once(':') {
        Some((host, port)) if !host.is_empty() => Some((host, port.parse().unwrap_or(443))),
        _ if !host_port.is_empty() => Some((host_port, 443)),
        _ => None,
    }
}

#[embassy_executor::task]
pub async fn run(stack: Stack<'static>, agent_config: &'static agent::AgentConfigSpace, runtime_config: &'static crate::runtime_config::RuntimeConfig, ota_config: &'static crate::ota::OtaConfigSpace, tls_config: &'static crate::tls::TlsConfigSpace, tls: TlsReferenceStatic) -> ! {
    // Declared once outside the reconnect loop, like `http::run`'s own
    // buffers -- reused across every reconnection attempt (not every
    // heartbeat -- there's only one connection attempt per many
    // heartbeats now). These are the raw TCP socket's own buffers (the
    // ciphertext in transit), not the application JSON buffer or the
    // response-draining buffer (`run_session`'s own, further down) -- a
    // TLS record larger than this just crosses it over more `read`/`write`
    // round trips instead of failing, so this is a throughput/RAM
    // tradeoff, not a correctness one.
    let mut rx_buffer = [0u8; 512];
    let mut tx_buffer = [0u8; 512];

    let mut last_ctrl_url_empty = None;
    loop {
        let ctrl_url = agent::ctrl_url(agent_config).await;
        if ctrl_url.is_empty() {
            // Logged once on the empty<->non-empty transition, not every
            // tick: contrat §2's "silence is worse than wrong" is about the
            // outbound wire, not the serial console, and five seconds of
            // log spam for the entire, possibly permanent, unprovisioned
            // lifetime of a device would drown out everything else.
            if last_ctrl_url_empty != Some(true) {
                info!("heartbeat: ctrl_url not provisioned, staying quiet");
            }
            last_ctrl_url_empty = Some(true);
        } else {
            if last_ctrl_url_empty != Some(false) {
                info!("heartbeat: ctrl_url={ctrl_url}, sending every {PERIOD:?} over one persistent connection");
            }
            last_ctrl_url_empty = Some(false);

            let token = agent::token(agent_config).await;
            if token.is_empty() {
                warn!("heartbeat: token not provisioned, staying quiet");
                Timer::after(PERIOD).await;
                continue;
            }

            if let Some((host, port)) = split_host_port(&ctrl_url)
                && let Ok(host_c) = CString::new(host)
            {
                if let Err(e) =
                    run_session(tls, stack, agent_config, runtime_config, ota_config, tls_config, &mut rx_buffer, &mut tx_buffer, &host_c, port, &ctrl_url, &token).await
                {
                    warn!("heartbeat: session ended: {e}");
                }
            }
        }
        Timer::after(PERIOD).await;
    }
}

/// Connects once, then sends heartbeats on that same connection every
/// [`PERIOD`] until something ends the session: a transport error, the
/// server declining keep-alive (explicitly via `Connection: close`, or
/// implicitly by sending a response this side can't safely re-sync after --
/// see `iobewi_http::client::drain_response`), or `ctrl_url` changing out from under it. `Ok`
/// and `Err` returns are both just "the caller should reconnect" -- the
/// distinction is only for `run`'s log line, not control flow.
async fn run_session(
    tls: TlsReferenceStatic,
    stack: Stack<'static>,
    agent_config: &'static agent::AgentConfigSpace,
    runtime_config: &'static crate::runtime_config::RuntimeConfig,
    ota_config: &'static crate::ota::OtaConfigSpace,
    tls_config: &'static crate::tls::TlsConfigSpace,
    rx_buffer: &mut [u8],
    tx_buffer: &mut [u8],
    host: &core::ffi::CStr,
    port: u16,
    ctrl_url_snapshot: &str,
    token_snapshot: &str,
) -> Result<(), String> {
    let mut session = crate::tls::connect_client(tls, stack, tls_config, crate::time::is_set(), rx_buffer, tx_buffer, host, port)
        .await
        .map_err(|e| format!("connect to {host:?}:{port} failed: {e}"))?;
    let host_str = host.to_str().unwrap_or("");
    info!("heartbeat: connected to {host_str}:{port}");

    // The response-draining scratch buffer: reused for every heartbeat on
    // this connection, distinct from `rx_buffer`/`tx_buffer` above (those
    // are the raw TLS record buffers; this is where the *decrypted*
    // HTTP/1.1 response headers, and any already-buffered start of the
    // body, land). 512B comfortably covers a small JSON ack's headers --
    // a response whose headers alone don't fit is treated as an error
    // (below), not silently truncated.
    let mut resp_buf = [0u8; 512];

    loop {
        // `ctrl_url` can change under us (pushed via the config API) --
        // checked once per heartbeat rather than once per connection, so a
        // change takes effect within one `PERIOD` instead of waiting for
        // the connection to drop on its own.
        if agent::ctrl_url(agent_config).await != ctrl_url_snapshot {
            info!("heartbeat: ctrl_url changed, closing this connection to reconnect to the new one");
            let _ = session.close().await;
            return Ok(());
        }
        if agent::token(agent_config).await != token_snapshot {
            info!("heartbeat: bearer token changed, closing this connection to reconnect with the new credential");
            let _ = session.close().await;
            return Ok(());
        }

        if let Err(e) = send_heartbeat(&mut session, agent_config, runtime_config, ota_config, stack, host_str, token_snapshot).await {
            return Err(format!("send to {host_str} failed: {e}"));
        }

        let (status, keep_alive) = match iobewi_http::client::drain_response(&mut session, &mut resp_buf).await {
            Ok(result) => result,
            Err(e) => return Err(format!("reading response from {host_str} failed: {e}")),
        };
        info!("heartbeat: POST {host_str}/v1alpha1/heartbeat -> {status}");
        if !keep_alive {
            info!("heartbeat: {host_str} ended keep-alive, reconnecting next tick");
            let _ = session.close().await;
            return Ok(());
        }

        Timer::after(PERIOD).await;
    }
}

/// Builds and sends one heartbeat POST on an already-connected `session`.
/// Doesn't read the response -- that's the HTTP client's job, kept
/// separate so a write failure and a read failure produce distinct log
/// context upstream.
async fn send_heartbeat<'h, 'buf>(
    session: &mut ClientStream<'h, 'buf>,
    agent_config: &'static agent::AgentConfigSpace,
    runtime_config: &'static crate::runtime_config::RuntimeConfig,
    ota_config: &'static crate::ota::OtaConfigSpace,
    stack: Stack<'static>,
    host_str: &str,
    token: &str,
) -> Result<(), String> {
    let node_id = agent::node_id(agent_config).await;
    let ip = stack.config_v4().map(|c| format!("{}", c.address.address())).unwrap_or_default();

    // `connect_client` refuses to connect before SNTP has converged (TLS
    // date validation fails closed), so the clock is set by here and the
    // contrat §5 `clock_unsynced` distress channel can no longer be sent:
    // an unsynced device is silent, and the log below says why.
    let ts = crate::time::now().unwrap_or_default();
    let reason = None;

    let state = agent::state();
    let body = Heartbeat {
        node_id: &node_id,
        ip: &ip,
        ts,
        state: state.as_str(),
        deployment_id: crate::ota::active_deployment_id(ota_config).await,
        firmware_digest: crate::ota::active_digest(ota_config).await,
        // contrat §3: distinguishes `pending_verify` (false) from every
        // other state -- the only state a heartbeat is still sent from
        // before `mark_valid` has run.
        ota_validated: state != agent::State::PendingVerify,
        config_generation: runtime_config.generation().await,
        reason,
        uptime_ms: Instant::now().as_millis(),
        heap_free: esp_alloc::HEAP.free() as u32,
        temp_celsius: TEMP_UNAVAILABLE,
        task_hwm_min: crate::stack_usage::free_bytes(),
    };
    let json = serde_json::to_string(&body).map_err(|_| String::from("heartbeat body failed to serialize"))?;

    iobewi_http::client::post_json(
        session, host_str, "/v1alpha1/heartbeat", token, &json,
    ).await?;
    Ok(())
}
