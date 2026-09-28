//! `PUT /v1alpha1/ota/write` (contrat §4): the one route that can't use
//! picoserve's usual `String`/`Form` body extractors -- those buffer the
//! *entire* body before a handler ever runs, and an OTA image (hundreds of
//! KB) doesn't fit this device's heap. Implements `RequestHandlerService`
//! directly instead, streaming the body straight into flash one `read()`
//! at a time.

use alloc::format;
use alloc::string::String;

use picoserve::io::Read;
use picoserve::request::Request;
use picoserve::response::{IntoResponse, ResponseWriter, StatusCode};
use picoserve::routing::RequestHandlerService;
use picoserve::ResponseSent;

use crate::{agent, ota};
use iobewi_esp_flash::SharedFlash;

use crate::http::{JsonResponse, json_error, json_ok, unauthorized};

/// Number of bytes carried by an inclusive HTTP Content-Range.
fn range_len(start: u32, end: u32) -> Option<u32> {
    end.checked_sub(start)?.checked_add(1)
}

/// Wire-format validation for X-Embewi-Digest.
fn is_valid_digest(value: &str) -> bool {
    iobewi_ota::metadata::parse_digest(value).is_some()
}

/// Parses Content-Range: bytes <start>-<end>/<total>.
///
/// This is deliberately HTTP-local. Resume/session decisions themselves
/// remain in IOBEWI OTA; only the wire syntax belongs to this route.
fn parse_content_range(value: &str) -> Option<(u32, u32, u32)> {
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start: u32 = start.trim().parse().ok()?;
    let end: u32 = end.trim().parse().ok()?;
    let total: u32 = total.trim().parse().ok()?;
    (start <= end && end < total).then_some((start, end, total))
}

pub struct OtaWrite {
    pub flash: &'static SharedFlash,
    pub ota_config: &'static ota::OtaConfigSpace,
    pub agent_config: &'static agent::AgentConfigSpace,
}

impl RequestHandlerService for OtaWrite {
    async fn call_request_handler_service<R: Read, W: ResponseWriter<Error = R::Error>>(
        &self,
        _state: &(),
        _path_parameters: (),
        mut request: Request<'_, R>,
        response_writer: W,
    ) -> Result<ResponseSent, W::Error> {
        let headers = request.parts.headers();
        let token = headers
            .get("authorization")
            .and_then(|v| v.as_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if !agent::is_authorized(self.agent_config, token).await {
            return unauthorized()
                .write_to(request.body_connection.finalize().await?, response_writer)
                .await;
        }

        let deployment_id = headers.get("x-embewi-deployment-id").and_then(|v| v.as_str().ok()).unwrap_or("");
        let digest = headers.get("x-embewi-digest").and_then(|v| v.as_str().ok()).unwrap_or("");
        let content_range = headers.get("content-range").and_then(|v| v.as_str().ok());
        let content_length = request.body_connection.content_length();

        // Refuse before touching the session: an invalid PUT must not
        // disturb one in progress. What a session is (deployment, digest,
        // total) is fixed by its first PUT and must be repeated verbatim.
        let bad_request = |error: &'static str| json_error(StatusCode::BAD_REQUEST, error);
        let invalid = if deployment_id.is_empty() {
            Some("{\"error\":\"missing_deployment_id\"}")
        } else if !is_valid_digest(digest) {
            Some("{\"error\":\"bad_digest\"}")
        } else {
            None
        };
        if let Some(error) = invalid {
            return bad_request(error)
                .write_to(request.body_connection.finalize().await?, response_writer)
                .await;
        }

        let (has_range, start, end, total) = match content_range {
            // Monolithic PUT: the body is the whole image.
            None => match u32::try_from(content_length).ok().filter(|len| *len > 0) {
                Some(len) => (false, 0u32, len - 1, len),
                None => {
                    return bad_request("{\"error\":\"empty_body\"}")
                        .write_to(request.body_connection.finalize().await?, response_writer)
                        .await;
                }
            },
            Some(value) => match parse_content_range(value) {
                Some((s, e, t)) if range_len(s, e).is_some_and(|len| len as usize == content_length) => {
                    (true, s, e, t)
                }
                Some(_) => {
                    return bad_request("{\"error\":\"content_length_mismatch\"}")
                        .write_to(request.body_connection.finalize().await?, response_writer)
                        .await;
                }
                None => {
                    return bad_request("{\"error\":\"bad_content_range\"}")
                        .write_to(request.body_connection.finalize().await?, response_writer)
                        .await;
                }
            },
        };
        let params = ota::SessionParams {
            deployment_id: String::from(deployment_id),
            digest: String::from(digest),
            total,
        };

        let in_progress = ota::write_in_progress().await;
        // The Continue-vs-Resync decision: how much this session has
        // *accepted* so far (flushed to flash or still buffered), which is
        // what an uninterrupted client's next chunk continues from --
        // distinct from `ota::write_written` (flushed only), reported to
        // the client below as the durable point to resume from after a
        // dropped connection.
        let received_so_far = ota::write_received().await;
        match ota::write_plan(has_range, start, in_progress, received_so_far) {
            ota::Plan::Begin => match ota::write_begin(self.flash, self.ota_config, params).await {
                Ok(()) => {}
                Err(ota::BeginError::TooLarge) => {
                    return json_error(StatusCode::PAYLOAD_TOO_LARGE, "{\"error\":\"size_too_large\"}")
                        .write_to(request.body_connection.finalize().await?, response_writer)
                        .await;
                }
                Err(ota::BeginError::Busy) => {
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "{\"status\":\"ota_begin_failed\"}")
                        .write_to(request.body_connection.finalize().await?, response_writer)
                        .await;
                }
                Err(ota::BeginError::Conflict) => {
                    return json_error(StatusCode::CONFLICT, "{\"error\":\"ota_busy\"}")
                        .write_to(request.body_connection.finalize().await?, response_writer)
                        .await;
                }
                Err(ota::BeginError::Storage(_)) => {
                    return json_error(StatusCode::INTERNAL_SERVER_ERROR, "{\"status\":\"nvs_write_failed\"}")
                        .write_to(request.body_connection.finalize().await?, response_writer)
                        .await;
                }
            },
            ota::Plan::Resync => {
                let written = ota::write_written().await;
                return json_error(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    &format!("{{\"error\":\"range_mismatch\",\"written\":{written}}}"),
                )
                .write_to(request.body_connection.finalize().await?, response_writer)
                .await;
            }
            ota::Plan::Continue => {
                if !ota::write_params_match(&params).await {
                    return json_error(StatusCode::CONFLICT, "{\"error\":\"session_mismatch\"}")
                        .write_to(request.body_connection.finalize().await?, response_writer)
                        .await;
                }
            }
        }

        let mut buf = [0u8; 1024];
        let mut remaining = content_length;
        let mut chunk_error = false;
        {
            let mut reader = request.body_connection.body().reader();
            while remaining > 0 {
                let to_read = remaining.min(buf.len());
                let n = reader.read(&mut buf[..to_read]).await?;
                if n == 0 {
                    chunk_error = true;
                    break;
                }
                if !ota::write_chunk(self.flash, &buf[..n]).await {
                    chunk_error = true;
                    break;
                }
                remaining -= n;
            }
        }

        if chunk_error {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, "{\"status\":\"write_failed\"}")
                .write_to(request.body_connection.finalize().await?, response_writer)
                .await;
        }

        if !ota::write_is_final(has_range, end, total) {
            let written = ota::write_written().await;
            return json_ok(format!("{{\"status\":\"partial\",\"written\":{written}}}"))
                .write_to(request.body_connection.finalize().await?, response_writer)
                .await;
        }

        let response: JsonResponse = match ota::write_finish(self.flash, self.ota_config).await {
            Ok(result) => json_ok(format!(
                "{{\"written\":{},\"digest\":\"{}\",\"status\":\"written\"}}",
                result.written, result.digest
            )),
            Err(ota::WriteFinishError::DigestMismatch) => json_ok(String::from("{\"status\":\"digest_mismatch\"}")),
            Err(ota::WriteFinishError::Storage(_)) => {
                json_error(StatusCode::INTERNAL_SERVER_ERROR, "{\"status\":\"nvs_write_failed\"}")
            }
            Err(ota::WriteFinishError::NotWriting | ota::WriteFinishError::Incomplete) => {
                json_error(StatusCode::INTERNAL_SERVER_ERROR, "{\"status\":\"write_failed\"}")
            }
        };
        response
            .write_to(request.body_connection.finalize().await?, response_writer)
            .await
    }
}
