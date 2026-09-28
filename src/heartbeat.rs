//! Heartbeat outbound flow (contrat §5). Device-initiated POST every 5s to
//! `ctrl_url + /v1alpha1/heartbeat`, so it keeps working even for a device
//! that never accepts inbound connections. Silent (skips the tick) while
//! `ctrl_url` is empty -- nothing provisioned yet, nothing to talk to.
//!
//! HTTPS via `tls::connect_client` (contrat §5: "le scheme est forcé en
//! https:// indépendamment du scheme stocké dans ctrl_url"). Hand-rolled
//! HTTP/1.1 request/response instead of `reqwless`: `reqwless`'s own TLS
//! support is hard-wired to a different crate (`embedded-tls`), and running
//! two separate TLS stacks in the same firmware just to keep `reqwless`
//! costs more flash than writing this one POST by hand costs in code.
//! Stays silent (no heartbeat sent, tick skipped) until a CA is configured
//! via `POST /v1alpha1/tls/ca` -- see `tls::ClientTlsError::NoCa`.
//!
//! **One TLS connection reused across many heartbeats**, not a fresh
//! handshake every 5s (the previous design: `Connection: close` on every
//! POST). At the old cadence that was 12 handshakes/minute, 720/hour,
//! 17 280/device/day -- pure overhead paid every tick, whether or not
//! anything changed. Structured as an outer reconnect loop around an inner
//! per-heartbeat loop (mirrors `log_stream.rs`'s `run`/`run_session`
//! split): the inner loop keeps sending on the same [`iobewi_esp_tls::mbedtls_rs::Session`]
//! until something actually requires a new connection -- the socket drops,
//! the server sends `Connection: close`, `ctrl_url` changes, or this
//! heartbeat's own framing can't be trusted to still be in sync (see
//! [`drain_response`]) -- at which point it returns and the outer loop
//! reconnects.
//!
//! The one thing that makes reuse safe at all: **fully draining every
//! response**, not just its status line. The old one-shot design could get
//! away with reading a handful of bytes and throwing the rest of the
//! connection away right after (it always closed next). On a kept-open
//! connection, any header or body byte left unread is exactly what the
//! *next* heartbeat's response parser would misread as the start of a new
//! one -- so [`drain_response`] parses `Content-Length` (or a minimal
//! `Transfer-Encoding: chunked` decoder) and consumes precisely that many
//! body bytes before this loop is allowed to send again. A response with
//! neither framing header present is genuinely ambiguous per HTTP/1.1(no
//! way to know where its body ends short of reading until the connection
//! closes) -- treated the same as an explicit `Connection: close`, not
//! guessed at.

use alloc::ffi::CString;
use alloc::format;
use alloc::string::String;

use embassy_net::Stack;
use embassy_net::tcp::TcpSocket;
use embassy_time::{Duration, Instant, Timer};
use log::{info, warn};
use iobewi_esp_tls::mbedtls_rs::Session;
use serde::Serialize;

use crate::agent;
use crate::tls::TlsReferenceStatic;

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
/// see [`drain_response`]), or `ctrl_url` changing out from under it. `Ok`
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
    let mut session = crate::tls::connect_client(tls, stack, tls_config, rx_buffer, tx_buffer, host, port)
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

        let (status, keep_alive) = match drain_response(&mut session, &mut resp_buf).await {
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
/// Doesn't read the response -- that's [`drain_response`]'s job, kept
/// separate so a write failure and a read failure produce distinct log
/// context upstream.
async fn send_heartbeat<'h, 'buf>(
    session: &mut Session<'h, TcpSocket<'buf>>,
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

    // No `Connection` header: HTTP/1.1 defaults to keep-alive, which is
    // exactly what this loop wants. (Explicitly `close`, the old
    // behaviour, is what this whole change replaces.)
    let request = format!(
        "POST /v1alpha1/heartbeat HTTP/1.1\r\n\
         Host: {host_str}\r\n\
         Authorization: Bearer {token}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n\
         {json}",
        json.len()
    );
    session.write(request.as_bytes()).await.map_err(|e| format!("write failed: {e}"))?;
    session.flush().await.map_err(|e| format!("flush failed: {e}"))?;
    Ok(())
}

/// Reads and fully consumes exactly one HTTP/1.1 response from `session`
/// (status line, headers, and -- critically for reuse -- its entire body),
/// returning the status code and whether this connection can safely be
/// reused for another request.
///
/// This is the one piece of code that makes keep-alive safe at all: the
/// old design only ever read a handful of bytes off a connection it was
/// about to close anyway, so it never had to account for the rest of a
/// response. Reused across many heartbeats, any header or body byte left
/// on the wire here is exactly what the *next* call would misparse as the
/// start of a new response.
///
/// Framing precedence (RFC 7230 §3.3.3): `Content-Length` if present, else
/// a minimal `Transfer-Encoding: chunked` decoder, else neither -- which
/// leaves no safe way to know where the body ends short of reading until
/// the connection closes, so that last case reports `keep_alive = false`
/// rather than guess. Same for an explicit `Connection: close`.
async fn drain_response<'h, 'buf>(
    session: &mut Session<'h, TcpSocket<'buf>>,
    resp_buf: &mut [u8],
) -> Result<(u16, bool), String> {
    let mut filled = 0;
    let header_end = loop {
        if filled >= resp_buf.len() {
            return Err(String::from("response headers too large for the scratch buffer"));
        }
        let n = session.read(&mut resp_buf[filled..]).await.map_err(|e| format!("read failed: {e}"))?;
        if n == 0 {
            return Err(String::from("connection closed while reading response headers"));
        }
        filled += n;
        if let Some(pos) = resp_buf[..filled].windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };

    let mut httparse_headers = [httparse::EMPTY_HEADER; 16];
    let mut response = httparse::Response::new(&mut httparse_headers);
    match response.parse(&resp_buf[..header_end]) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => return Err(String::from("response headers unexpectedly incomplete")),
        Err(e) => return Err(format!("response parse failed: {e:?}")),
    }
    let status = response.code.unwrap_or(0);

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let mut keep_alive = true; // HTTP/1.1 default, absent a `Connection` header saying otherwise.
    for h in response.headers.iter() {
        let Ok(value) = core::str::from_utf8(h.value) else { continue };
        if h.name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().ok();
        } else if h.name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.to_ascii_lowercase().contains("chunked");
        } else if h.name.eq_ignore_ascii_case("connection") && value.to_ascii_lowercase().contains("close") {
            keep_alive = false;
        }
    }

    // Small scratch space for discarding body bytes this loop has no use
    // for reading into `resp_buf` itself -- the point is just to advance
    // past them on the wire, not to keep them.
    let mut discard = [0u8; 128];

    if let Some(len) = content_length {
        let body_so_far = filled - header_end;
        let mut remaining = len.saturating_sub(body_so_far);
        while remaining > 0 {
            let to_read = remaining.min(discard.len());
            let n = session.read(&mut discard[..to_read]).await.map_err(|e| format!("body read failed: {e}"))?;
            if n == 0 {
                return Err(String::from("connection closed mid-body"));
            }
            remaining -= n;
        }
    } else if chunked {
        // Compacts whatever body bytes already arrived with the headers to
        // the front of `resp_buf`, then decodes in place: `carry_len` is
        // how many *unprocessed* bytes are sitting at `resp_buf[..carry_len]`
        // (chunk-size lines, chunk data, or trailers not yet consumed).
        // Capacity matches `resp_buf` exactly, so nothing already read off
        // the wire can be dropped the way a separately-sized buffer might.
        let mut carry_len = filled - header_end;
        resp_buf.copy_within(header_end..filled, 0);

        loop {
            let line_end = loop {
                if let Some(pos) = resp_buf[..carry_len].windows(2).position(|w| w == b"\r\n") {
                    break pos;
                }
                if carry_len >= resp_buf.len() {
                    return Err(String::from("chunk size line too long for the scratch buffer"));
                }
                let n = session.read(&mut resp_buf[carry_len..]).await.map_err(|e| format!("chunk read failed: {e}"))?;
                if n == 0 {
                    return Err(String::from("connection closed mid-chunk-size"));
                }
                carry_len += n;
            };
            let size_field = core::str::from_utf8(&resp_buf[..line_end]).map_err(|_| String::from("chunk size line isn't valid UTF-8"))?;
            let size_field = size_field.split(';').next().unwrap_or(""); // drop chunk extensions, if any
            let size =
                usize::from_str_radix(size_field.trim(), 16).map_err(|_| format!("bad chunk size {size_field:?}"))?;

            let after_line = line_end + 2; // the chunk-size line's own trailing CRLF
            resp_buf.copy_within(after_line..carry_len, 0);
            carry_len -= after_line;

            if size == 0 {
                // Final chunk: consume the trailer section (usually just
                // one more CRLF, but RFC 7230 allows trailer headers) up
                // to its terminating blank line, then this response is
                // fully drained.
                loop {
                    if resp_buf[..carry_len].windows(4).position(|w| w == b"\r\n\r\n").is_some()
                        || (carry_len >= 2 && &resp_buf[..2] == b"\r\n")
                    {
                        break;
                    }
                    if carry_len >= resp_buf.len() {
                        return Err(String::from("chunked trailer too long for the scratch buffer"));
                    }
                    let n = session.read(&mut resp_buf[carry_len..]).await.map_err(|e| format!("trailer read failed: {e}"))?;
                    if n == 0 {
                        return Err(String::from("connection closed mid-trailer"));
                    }
                    carry_len += n;
                }
                break;
            }

            let mut remaining = size + 2; // chunk data plus its own trailing CRLF
            let take = remaining.min(carry_len);
            resp_buf.copy_within(take..carry_len, 0);
            carry_len -= take;
            remaining -= take;
            while remaining > 0 {
                let to_read = remaining.min(discard.len());
                let n =
                    session.read(&mut discard[..to_read]).await.map_err(|e| format!("chunk data read failed: {e}"))?;
                if n == 0 {
                    return Err(String::from("connection closed mid-chunk-data"));
                }
                remaining -= n;
            }
        }
    } else {
        // Neither `Content-Length` nor `Transfer-Encoding: chunked`: this
        // response's body (if any) is only delimited by the connection
        // closing, which this side can't safely wait for without breaking
        // its own `PERIOD` cadence. Not an error -- just not safe to keep
        // this connection open for another request.
        keep_alive = false;
    }

    Ok((status, keep_alive))
}
