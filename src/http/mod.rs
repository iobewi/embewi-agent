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

use picoserve::response::StatusCode;
pub(super) use iobewi_http::json::{json_error, json_ok, JsonResponse};
use iobewi_http::routing::PathRouter;
use iobewi_http::HttpRouter;

pub mod api;
pub mod config;

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

/// Shared by every `/v1alpha1/*` handler (contrat §4b: `401
/// {"error":"unauthorized"}`).
pub(super) fn unauthorized() -> JsonResponse {
    json_error(StatusCode::UNAUTHORIZED, "{\"error\":\"unauthorized\"}")
}

/// The runtime and provisioning routers share the IOBEWI HTTP server. The
/// caller supplies an already-constructed, already-authenticated TLS
/// listener -- this layer knows only "serve this router over TLS
/// connections accepted by `listener`", never which concrete TLS stack or
/// hardware produced them.
pub(super) async fn serve<L: iobewi_https::TlsListener>(
    listener: &mut L,
    router: &HttpRouter<impl PathRouter>,
) -> ! {
    iobewi_https::serve_forever(listener, router).await
}
