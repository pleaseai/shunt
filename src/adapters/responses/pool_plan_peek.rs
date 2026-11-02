//! The pre-content check on a chatgpt-plan pool account's 2xx answer
//! (ADR-0007 Decision 3).
//!
//! The SIWC preview reports some account failures (usage limit, invalid user,
//! unavailable) as a `response.failed` or `error` event on a `200 OK` stream
//! instead of an HTTP status. Before any model output, that failure is the
//! account's, not the turn's, so the pool should rotate exactly as it does on
//! the HTTP status.
//!
//! [`peek_pre_content`] reads the stream only as far as the first frame that
//! is not a lifecycle frame (`response.created`, `response.in_progress`,
//! `response.queued`). Those frames carry no model output, and the Anthropic
//! client already has its synthetic `message_start`, so holding them does not
//! delay anything the client could see (NFR-1).
//!
//! - When that first frame is a failure whose code
//!   [`crate::model::siwc_errors::lookup`] maps, the held bytes are dropped
//!   and the pool sees a synthetic response of the mapped status whose body is
//!   the event's own JSON, which the existing error mapping reads.
//! - Otherwise, including when the stream ends, errors, or reaches
//!   [`PEEK_LIMIT`] first, the held bytes are replayed byte-identical,
//!   followed by the rest of the stream untouched.

use axum::http::{header, HeaderValue, StatusCode};
use bytes::{Bytes, BytesMut};
use futures_util::{stream, StreamExt};
use serde_json::Value;

use crate::model::{
    responses::{error_code, ResponseEvent},
    siwc_errors,
};

use super::sse_parse::SseParser;

/// How many bytes of lifecycle frames are held before the check gives up and
/// replays them.
pub(super) const PEEK_LIMIT: usize = 64 * 1024;

/// The frames that may be held: they open the response and carry no output.
const LIFECYCLE_EVENTS: [&str; 3] = [
    "response.created",
    "response.in_progress",
    "response.queued",
];

/// What the first non-lifecycle frame decided.
enum Verdict {
    /// Only lifecycle frames so far: keep reading.
    Hold,
    /// Relay the stream untouched.
    Replay,
    /// A mapped SIWC failure: answer with this status and the event's JSON.
    Fail(StatusCode, Value),
}

/// Run the pre-content check on `upstream`. A non-2xx response is returned
/// unread.
pub(super) async fn peek_pre_content(upstream: reqwest::Response) -> reqwest::Response {
    if !upstream.status().is_success() {
        return upstream;
    }
    let status = upstream.status();
    let version = upstream.version();
    let mut headers = upstream.headers().clone();
    let extensions = upstream.extensions().clone();
    let mut rest = upstream.bytes_stream();
    let mut parser = SseParser::default();
    let mut held = BytesMut::new();
    let mut transport_error = None;
    loop {
        match rest.next().await {
            None => break,
            Some(Err(error)) => {
                transport_error = Some(error);
                break;
            }
            Some(Ok(chunk)) => {
                held.extend_from_slice(&chunk);
                match judge(&mut parser, &chunk) {
                    Verdict::Hold if held.len() < PEEK_LIMIT => {}
                    Verdict::Hold | Verdict::Replay => break,
                    Verdict::Fail(failed, event) => {
                        tracing::debug!(
                            status = %failed,
                            code = error_code(&event),
                            "chatgpt-plan stream failed before any output; classifying it as an HTTP status"
                        );
                        headers.remove(header::CONTENT_LENGTH);
                        headers.remove(header::TRANSFER_ENCODING);
                        headers.insert(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/json"),
                        );
                        let body = reqwest::Body::from(event.to_string());
                        return rebuild(body, failed, version, headers, extensions);
                    }
                }
            }
        }
    }
    let head = (!held.is_empty()).then(|| Ok(held.freeze()));
    let body = match transport_error {
        // A failed read ends the stream; nothing is polled after it.
        Some(error) => reqwest::Body::wrap_stream(stream::iter(
            head.into_iter().chain(std::iter::once(Err(error))),
        )),
        None => reqwest::Body::wrap_stream(stream::iter(head).chain(rest)),
    };
    rebuild(body, status, version, headers, extensions)
}

/// Feed one chunk to the SSE parser and judge the frames it completed.
fn judge(parser: &mut SseParser, chunk: &Bytes) -> Verdict {
    let (events, malformed) = parser.push(chunk);
    for event in events {
        match judge_event(event) {
            Verdict::Hold => continue,
            decided => return decided,
        }
    }
    // A malformed frame is left for the relay to report.
    if malformed {
        Verdict::Replay
    } else {
        Verdict::Hold
    }
}

fn judge_event(event: ResponseEvent) -> Verdict {
    let name = event
        .event
        .as_deref()
        .or_else(|| event.data.get("type").and_then(Value::as_str))
        .unwrap_or_default();
    if LIFECYCLE_EVENTS.contains(&name) {
        return Verdict::Hold;
    }
    if !matches!(name, "response.failed" | "error") {
        return Verdict::Replay;
    }
    match siwc_errors::lookup(error_code(&event.data)) {
        Some(mapped) => Verdict::Fail(mapped.status, event.data),
        None => Verdict::Replay,
    }
}

fn rebuild(
    body: reqwest::Body,
    status: StatusCode,
    version: axum::http::Version,
    headers: axum::http::HeaderMap,
    extensions: axum::http::Extensions,
) -> reqwest::Response {
    let mut rebuilt = axum::http::Response::new(body);
    *rebuilt.status_mut() = status;
    *rebuilt.version_mut() = version;
    *rebuilt.headers_mut() = headers;
    *rebuilt.extensions_mut() = extensions;
    reqwest::Response::from(rebuilt)
}
