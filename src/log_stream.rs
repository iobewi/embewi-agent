//! ESP_LOGx streaming to the Core over an outbound client WebSocket
//! (contrat §5). Captures every log line through a custom [`log::Log`]
//! implementation installed in place of `esp_println`'s own (which exposes
//! no hook to duplicate lines through -- see [`install`]), buffers them in
//! a small ring, and a background task drains the ring into
//! `wss://ctrl_url_host:port/v1alpha1/logs`.
//!
//! HTTP upgrade, authentication framing and WebSocket frames are handled
//! by the portable HTTP client; the agent supplies log data and scheduling.
//!
//! Best-effort by design (contrat §5, "on préfère perdre des logs plutôt
//! que bloquer le système"): a full ring drops the newest line rather than
//! blocking whoever is logging, and a disconnected/reconnecting socket
//! drains-and-discards rather than backlogging -- streaming *recent* logs
//! on reconnect beats replaying a stale backlog.

use alloc::ffi::CString;
use alloc::format;
use alloc::string::String as AllocString;
use core::cell::RefCell;
use core::fmt::Write as _;

use critical_section::Mutex;
use embassy_net::Stack;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use heapless::{Deque, String as HString};
use log::{Level, LevelFilter, Metadata, Record, info, warn};
use serde::Serialize;

use crate::agent;
use crate::tls::{ClientStream, TlsReferenceStatic};

const LINE_MAX: usize = 160;
const RING_CAPACITY: usize = 24;
const DRAIN_PERIOD: Duration = Duration::from_millis(200);

/// Reconnect backoff for the outer loop in [`run`]: 5s, 10s, 20s, 40s,
/// 60s, 60s... (doubling, capped at `MAX_BACKOFF`) -- before this, any
/// failure (a durably unreachable Core, `NoCa`, a bad cert, anything)
/// meant a fresh DNS+TCP+TLS handshake attempt every 5s indefinitely.
const BASE_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A session that stayed up for at least this long is "was healthy, just
/// glitched", not "still broken" -- [`run`] resets the backoff to
/// `BASE_BACKOFF` after one, so a normally-stable Core recovers quickly
/// from a one-off blip while a durably broken one (bad CA, TLS
/// misconfigured, Core down for good) doesn't get hammered every
/// `BASE_BACKOFF` forever. Deliberately one flat threshold/backoff for
/// every failure mode (`ClientTlsError`'s variants included) rather than a
/// different schedule per cause -- keeps this change small; nothing here
/// rules out splitting that out later if a specific cause turns out to
/// need it.
const STABLE_THRESHOLD: Duration = Duration::from_secs(30);

/// `current * 2`, capped at [`MAX_BACKOFF`]. Never below `current` (so
/// repeated calls actually converge), which for the smallest possible
/// `current` this is ever called with (`BASE_BACKOFF`) already holds.
fn next_backoff(current: Duration) -> Duration {
    Duration::from_secs((current.as_secs() * 2).min(MAX_BACKOFF.as_secs()))
}

/// `base` ± up to 30%, drawn from the same hardware RNG the WS frame
/// masking below already uses. Without this, a whole fleet of devices that
/// lost the same Core at the same instant (a Core restart, a network
/// blip) would retry in lockstep and hit it with a burst the moment it
/// comes back, instead of spreading back out over roughly the backoff
/// window.
fn jittered(base: Duration) -> Duration {
    let base_ms = base.as_millis() as i64;
    let percent = (esp_hal::rng::Rng::new().random() % 61) as i64 - 30; // -30..=30
    let ms = (base_ms + base_ms * percent / 100).max(1000);
    Duration::from_millis(ms as u64)
}

static RING: Mutex<RefCell<Deque<HString<LINE_MAX>, RING_CAPACITY>>> =
    Mutex::new(RefCell::new(Deque::new()));

struct Logger;

impl log::Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        // Mirrors .cargo/config.toml's ESP_LOG="warn,embewi_agent_esp=info"
        // -- hardcoded rather than re-parsed at runtime, since
        // esp_println's own env-filter machinery is generated into that
        // crate's own OUT_DIR and isn't reusable from outside it (see this
        // module's doc comment: there's no hook to chain onto its logger,
        // so this one replaces it, filter included).
        if metadata.target().starts_with("embewi_agent_esp") {
            metadata.level() <= Level::Info
        } else {
            metadata.level() <= Level::Warn
        }
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Replaces esp_println's logger entirely -- this is the only
        // place these lines get printed locally, not a second sink next
        // to it.
        esp_println::println!("{} - {}", record.level(), record.args());

        let mut line: HString<LINE_MAX> = HString::new();
        let fits = write!(line, "{}", record.args()).is_ok();
        // Too long to fit: drop rather than send a truncated/corrupt line.
        if !fits {
            return;
        }
        critical_section::with(|cs| {
            let mut ring = RING.borrow(cs).borrow_mut();
            if ring.is_full() {
                return;
            }
            let _ = ring.push_back(line);
        });
    }

    fn flush(&self) {}
}

static LOGGER: Logger = Logger;

/// Installs this module's logger in place of `esp_println`'s own. Call
/// once, at boot, instead of `esp_println::logger::init_logger_from_env()`.
pub fn install() {
    unsafe {
        let _ = log::set_logger_racy(&LOGGER);
        log::set_max_level_racy(LevelFilter::Info);
    }
}

fn pop_line() -> Option<HString<LINE_MAX>> {
    critical_section::with(|cs| RING.borrow(cs).borrow_mut().pop_front())
}

fn drain_and_discard() {
    critical_section::with(|cs| RING.borrow(cs).borrow_mut().clear());
}

#[derive(Serialize)]
struct LogFrame<'a> {
    ts: u64,
    node: &'a str,
    workload: &'static str,
    level: &'static str,
    msg: &'a str,
}

/// `ctrl_url` is `scheme://host[:port]` (the scheme itself is ignored --
/// contrat forces `wss://` regardless of what's stored, same as
/// `heartbeat.rs`). `None` on a malformed value rather than panicking this
/// task.
fn split_host_port(ctrl_url: &str) -> Option<(&str, u16)> {
    let rest = ctrl_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(ctrl_url);
    let host_port = rest.split('/').next().unwrap_or(rest);
    match host_port.split_once(':') {
        Some((host, port)) if !host.is_empty() => Some((host, port.parse().unwrap_or(443))),
        _ if !host_port.is_empty() => Some((host_port, 443)),
        _ => None,
    }
}

#[embassy_executor::task]
pub async fn run(stack: Stack<'static>, agent_config: &'static agent::AgentConfigSpace, tls_config: &'static crate::tls::TlsConfigSpace, tls: TlsReferenceStatic) -> ! {
    // Declared once outside the reconnect loop, like `http::run`'s own
    // buffers -- reused across every reconnection attempt. Raw TCP socket
    // buffers (the ciphertext in transit), not the WS frame payload itself
    // (`LINE_MAX`-bounded, well under 512B) -- sized down from 2048/1024
    // the same way `heartbeat.rs`'s were, for the same reason (see that
    // module's comment): shrinks this task's `Future`, and a larger TLS
    // record just crosses over more round trips instead of failing.
    let mut rx_buffer = [0u8; 1024];
    let mut tx_buffer = [0u8; 512];
    let mut backoff = BASE_BACKOFF;

    loop {
        let ctrl_url = agent::ctrl_url(agent_config).await;
        let Some((host, port)) = split_host_port(&ctrl_url) else {
            // Nothing provisioned (or malformed) -- not a failure, so this
            // doesn't touch `backoff` at all, only how long until the next
            // check.
            drain_and_discard();
            Timer::after(BASE_BACKOFF).await;
            continue;
        };

        let token = agent::token(agent_config).await;
        if token.is_empty() {
            drain_and_discard();
            Timer::after(BASE_BACKOFF).await;
            continue;
        }
        let Ok(host_c) = CString::new(host) else {
            drain_and_discard();
            Timer::after(BASE_BACKOFF).await;
            continue;
        };

        let stable = match connect_and_upgrade(tls, stack, tls_config, &mut rx_buffer, &mut tx_buffer, &host_c, port, &token).await {
            Ok(mut session) => {
                let connected_at = Instant::now();
                let err = pump_session(&mut session, agent_config, &token).await;
                warn!("logs: session ended: {err}");
                connected_at.elapsed() >= STABLE_THRESHOLD
            }
            Err(e) => {
                warn!("logs: session ended: {e}");
                false
            }
        };

        drain_and_discard();
        if stable {
            backoff = BASE_BACKOFF;
        }
        Timer::after(jittered(backoff)).await;
        backoff = next_backoff(backoff);
    }
}

/// Connects and completes the WS upgrade handshake -- everything [`run`]
/// needs before a session is ready to [`pump_session`]. A failure here
/// never counts as "was stable" (see [`STABLE_THRESHOLD`]): there's no
/// [`Instant`] to measure from since nothing ever connected.
async fn connect_and_upgrade<'h, 'buf>(
    tls: TlsReferenceStatic,
    stack: Stack<'static>,
    tls_config: &'static crate::tls::TlsConfigSpace,
    rx_buffer: &'buf mut [u8],
    tx_buffer: &'buf mut [u8],
    host: &'h core::ffi::CStr,
    port: u16,
    token: &str,
) -> Result<ClientStream<'h, 'buf>, AllocString> {
    let mut session = crate::tls::connect_client(tls, stack, tls_config, crate::time::is_set(), rx_buffer, tx_buffer, host, port)
        .await
        .map_err(|e| format!("connect to {host:?}:{port} failed: {e}"))?;

    let host_str = host.to_str().unwrap_or("");
    let mut nonce = [0u8; iobewi_http::websocket::NONCE_LENGTH];
    esp_hal::rng::Rng::new().read(&mut nonce);
    iobewi_http::websocket::upgrade(&mut session, host_str, "/v1alpha1/logs", token, &nonce).await?;
    info!("logs: connected to {host_str}:{port}");
    Ok(session)
}

/// Drives an already-upgraded WS session until it ends, always returning a
/// message describing why (a clean server-initiated close included --
/// there's no "done" state for a log stream short of that). The framework
/// handles frames and ping/pong; this task controls when to drain the ring.
async fn pump_session<'h, 'buf>(
    session: &mut ClientStream<'h, 'buf>,
    agent_config: &'static agent::AgentConfigSpace,
    token_snapshot: &str,
) -> AllocString {
    loop {
        // The Bearer credential is bound to the HTTP upgrade request. If a
        // token rotation lands while this WebSocket is alive, reconnect so
        // the next upgrade is authenticated with the new credential instead
        // of keeping an already-authenticated old-token session indefinitely.
        if agent::token(agent_config).await != token_snapshot {
            return AllocString::from("bearer token changed, reconnecting");
        }
        // Bounded read for whatever the server sent, mainly keepalive
        // Pings: a write-only client that never reads never answers them,
        // and real WS servers (confirmed against Python's `websockets`)
        // drop the connection over that alone, RFC 6455 requiring a Pong
        // in response. `DRAIN_PERIOD` doubles as the read timeout, so this
        // doesn't delay draining the ring by more than that either way.
        let mask_key = esp_hal::rng::Rng::new().random();
        match with_timeout(DRAIN_PERIOD, iobewi_http::websocket::process_frame(&mut *session, mask_key)).await {
            Ok(Ok(true)) | Err(_) => {}
            Ok(Ok(false)) => return AllocString::from("server closed the connection"),
            Ok(Err(e)) => return e,
        }

        while let Some(line) = pop_line() {
            let node_id = agent::node_id(agent_config).await;
            let ts = crate::time::now().unwrap_or(0);
            let frame =
                LogFrame { ts, node: &node_id, workload: agent::FW_NAME, level: "raw", msg: &line };
            let Ok(json) = serde_json::to_vec(&frame) else {
                continue;
            };
            let mask_key = esp_hal::rng::Rng::new().random();
            if let Err(e) = iobewi_http::websocket::send_text(&mut *session, &json, mask_key).await {
                return e;
            }
        }
    }
}
