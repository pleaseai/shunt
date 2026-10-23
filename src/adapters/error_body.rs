//! Bounded read of an upstream error body that an adapter must judge before it
//! relays the response — the Codex pool's per-account model refusal and the
//! Anthropic adapter's account-level refusal both decide on the body, and both
//! still owe the client the upstream's own bytes.

use std::time::Duration;

use futures_util::{stream, StreamExt};

use super::AdapterError;

/// Read the head of an upstream error body so it can be checked for a
/// refusal. The read is bounded like the Responses adapter's error envelope
/// read ([`crate::error::ERROR_ENVELOPE_BUDGET`] and
/// [`crate::error::ERROR_ENVELOPE_BYTES`]), plus the gated idle gap when the
/// caller has one (`ResponseBounds::idle`, as `json_response` honours it):
/// `clock` is that gap and the attempt's send, so the first wait is the rest
/// of the gap the header wait began rather than a fresh one, and each chunk
/// refreshes it (#704). The
/// bytes are returned only when the whole body arrived within those bounds: a
/// body that declares or passes the cap, trips the budget, or breaks is not a
/// refusal, and is passed on unjudged. Only an idle cut is an error: it is the
/// caller's bound, carried back to `routing::serve` as its marker. With a
/// clock, a budget trip before the body sent a byte is that cut too (#710).
///
/// What an unjudged body becomes depends on who relays it. `verbatim` (the
/// inbound passthrough) replays every byte read followed by whatever the
/// upstream has not sent yet — a declared-oversized body is not read at all —
/// so the client gets the upstream's 400 exactly as it was sent. Otherwise the
/// body is the text the envelope read falls back to (`upstream returned
/// {status}`): the translating path would render that anyway, and handing it
/// the unread remainder would restart the envelope read's budget, outside the
/// idle gap, on a body that has already spent both.
pub(crate) async fn buffer_error_body(
    upstream: reqwest::Response,
    clock: Option<(Duration, tokio::time::Instant)>,
    verbatim: bool,
) -> Result<(reqwest::Response, Option<bytes::Bytes>), AdapterError> {
    buffer_error_body_capped(
        upstream,
        clock,
        verbatim,
        crate::error::ERROR_ENVELOPE_BUDGET,
        None,
    )
    .await
}

/// [`buffer_error_body`] under a caller-chosen `budget` in place of
/// [`crate::error::ERROR_ENVELOPE_BUDGET`], for a caller that must decide
/// sooner than the envelope read allows. It also honours the caller's own byte
/// cap: the read stops at the smaller of `cap` and
/// [`crate::error::ERROR_ENVELOPE_BYTES`], and a body that declares or passes
/// that limit is passed on unjudged, so the relay that follows still applies
/// the caller's cap to it exactly as it would have without this read.
pub(crate) async fn buffer_error_body_capped(
    upstream: reqwest::Response,
    clock: Option<(Duration, tokio::time::Instant)>,
    verbatim: bool,
    budget: Duration,
    cap: Option<usize>,
) -> Result<(reqwest::Response, Option<bytes::Bytes>), AdapterError> {
    let limit = cap.map_or(crate::error::ERROR_ENVELOPE_BYTES, |cap| {
        cap.min(crate::error::ERROR_ENVELOPE_BYTES)
    });
    let declared_oversized = upstream
        .content_length()
        .is_some_and(|length| length > limit as u64);
    if declared_oversized && verbatim {
        return Ok((upstream, None));
    }
    let status = upstream.status();
    let version = upstream.version();
    let mut headers = upstream.headers().clone();
    let mut rest = upstream.bytes_stream();
    let budget_ends = tokio::time::Instant::now() + budget;
    let mut read = Vec::new();
    let mut total = 0usize;
    let mut idle_deadline = clock.map(|(idle, since)| since + idle);
    let complete = !declared_oversized
        && loop {
            let wait = idle_deadline.map_or(budget_ends, |deadline| deadline.min(budget_ends));
            match tokio::time::timeout_at(wait, rest.next()).await {
                Ok(None) => break true,
                Ok(Some(Ok(chunk))) => {
                    idle_deadline = clock.map(|(idle, _)| tokio::time::Instant::now() + idle);
                    total = total.saturating_add(chunk.len());
                    read.push(Ok(chunk));
                    if total > limit {
                        break false;
                    }
                }
                Ok(Some(Err(error))) => {
                    read.push(Err(error));
                    break false;
                }
                Err(_) => match (clock, idle_deadline) {
                    (Some((idle, _)), Some(deadline)) if deadline < budget_ends => {
                        return Err(super::idle_error(super::UpstreamBodyIdle { idle }));
                    }
                    // The budget ended a body that never sent a byte: on a
                    // gated call that is a stall too, as
                    // `bounded_upstream_text_within` cuts it (#710).
                    (Some(_), _) if total == 0 => {
                        return Err(super::idle_error(super::UpstreamBodyIdle { idle: budget }));
                    }
                    _ => break false,
                },
            }
        };
    let (body, judged) = if complete {
        let mut bytes = bytes::BytesMut::with_capacity(total);
        for chunk in read.iter().flatten() {
            bytes.extend_from_slice(chunk);
        }
        let bytes = bytes.freeze();
        (reqwest::Body::from(bytes.clone()), Some(bytes))
    } else if verbatim {
        (
            reqwest::Body::wrap_stream(stream::iter(read).chain(rest)),
            None,
        )
    } else {
        headers.remove(reqwest::header::CONTENT_LENGTH);
        (
            reqwest::Body::from(format!("upstream returned {status}")),
            None,
        )
    };
    let mut rebuilt = axum::http::Response::new(body);
    *rebuilt.status_mut() = status;
    *rebuilt.version_mut() = version;
    *rebuilt.headers_mut() = headers;
    Ok((reqwest::Response::from(rebuilt), judged))
}
