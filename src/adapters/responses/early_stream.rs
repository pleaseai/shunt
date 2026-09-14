//! The Responses adapter's early-commit streaming machinery: commit the SSE
//! response with a synthetic `message_start` before any upstream byte, drive
//! the upstream feed (send, retry, parsing) inside the stream, and turn every
//! pre-stream failure into one terminal Anthropic SSE `error` event.

use std::convert::Infallible;

use axum::{
    body::{Body, Bytes},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::{stream, Stream, StreamExt};
use serde_json::Value;

use crate::{
    auth::Credential,
    config::AuthMode,
    model::responses::{sse, AnthropicSseMachine, ResponseEvent},
    routing::Route,
    server::AppState,
};

use super::body::PreparedBody;
use super::error::{adapter_error_envelope, mapped_upstream_error, own_error, transport_error};
use super::http::http_send;

/// The streaming response for the early-commit transport: emit the synthetic
/// `message_start` + initial ping immediately — before any upstream byte — and
/// then relay the translated events. The keepalive wrapper spans both phases,
/// so the client hop is never silent for longer than `keepalive` even while
/// the upstream thinks in silence.
pub(super) fn early_streaming_response(
    mut machine: AnthropicSseMachine,
    keepalive: std::time::Duration,
    events: impl Stream<Item = Result<ResponseEvent, Value>> + Send + 'static,
) -> axum::response::Response {
    let start = machine.start_synthetic(format!("msg_{}", uuid::Uuid::new_v4()));
    let output = stream::once(async move { Ok::<Bytes, Infallible>(Bytes::from(start.join(""))) })
        .chain(translated_stream(events, machine));
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(crate::keepalive::with_pings(
            output, keepalive,
        )))
        .expect("response builder uses valid status and headers")
        .into_response()
}

/// The pooled variant of [`early_streaming_response`]: the same synthetic
/// start, then [`pool_translated_stream`] so the winning account's `account`
/// frame lands before its first relayed frame. The `x-shunt-account` header
/// the non-streaming path still sets cannot ride the early-commit response —
/// headers go out before the winner is known — so the attribution moves into
/// the stream.
pub(super) fn pool_streaming_response(
    mut machine: AnthropicSseMachine,
    keepalive: std::time::Duration,
    events: impl Stream<Item = Result<PoolEvent, Value>> + Send + 'static,
) -> axum::response::Response {
    let start = machine.start_synthetic(format!("msg_{}", uuid::Uuid::new_v4()));
    let output = stream::once(async move { Ok::<Bytes, Infallible>(Bytes::from(start.join(""))) })
        .chain(pool_translated_stream(events, machine));
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(crate::keepalive::with_pings(
            output, keepalive,
        )))
        .expect("response builder uses valid status and headers")
        .into_response()
}

type UpstreamBytes = std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

/// The raw outcome of one bounded-retry send, before the status is judged.
/// Split out of [`http_events_stream`] so the multi-upstream chain
/// (`proxy::chain_stream`) can classify a failure as advance-worthy without
/// duplicating the send, the quota capture, or the envelope building.
pub(super) enum SendClassified {
    Relay { bytes: UpstreamBytes },
    Failed { envelope: Value, status: StatusCode },
}

/// Drive one bounded-retry send and classify the raw outcome. A retry-exhausted
/// transport error becomes a gateway error envelope with `502` (the pre-commit
/// path's shape); a non-2xx upstream status becomes the mapped envelope for
/// that status. The caller decides whether a failure advances a chain.
pub(super) async fn send_classified(context: &HttpSendContext) -> SendClassified {
    let outcome = crate::retry::send_with_retry_with_safety(
        context.policy,
        &context.route.provider,
        crate::retry::RetrySafety::NonIdempotentPost,
        || {
            http_send(
                &context.state,
                &context.route,
                context.credential.clone(),
                context.session_id.as_deref(),
                context.body.clone(),
            )
        },
    )
    .await;
    let upstream = match outcome {
        Ok(response) => response,
        Err(error) => {
            let envelope = adapter_error_envelope(
                error.into_adapter_error(|error| transport_error(error.without_url().to_string())),
            )
            .await;
            return SendClassified::Failed {
                envelope,
                status: StatusCode::BAD_GATEWAY,
            };
        }
    };
    if let Some(account) = &context.codex_quota_account {
        context.state.accounts.note_codex_quota(
            &context.route.provider,
            account,
            upstream.headers(),
        );
    }
    if !upstream.status().is_success() {
        let status = upstream.status();
        let envelope =
            adapter_error_envelope(mapped_upstream_error(status, upstream, context.auth).await)
                .await;
        return SendClassified::Failed { envelope, status };
    }
    SendClassified::Relay {
        bytes: Box::pin(upstream.bytes_stream()),
    }
}

/// Everything [`http_events_stream`] needs to drive one upstream send.
#[derive(Clone)]
pub(super) struct HttpSendContext {
    pub(super) state: AppState,
    pub(super) route: Route,
    pub(super) policy: crate::retry::RetryPolicy,
    pub(super) credential: Credential,
    pub(super) session_id: Option<String>,
    pub(super) body: PreparedBody,
    pub(super) auth: AuthMode,
    pub(super) codex_quota_account: Option<crate::config::AccountConfig>,
}

/// One streaming turn's upstream feed: drive the bounded-retry send inside the
/// stream (the response is already committed), capture the x-codex-* quota
/// windows, and turn every pre-stream failure — a retry-exhausted transport
/// error, the TTFB timeout, or a non-2xx status — into the same Anthropic
/// error envelope the pre-commit path returned as a JSON body, now emitted as
/// one terminal SSE `error` event by [`early_streaming_response`].
pub(super) fn http_events_stream(
    context: HttpSendContext,
) -> impl Stream<Item = Result<ResponseEvent, Value>> + Send + 'static {
    enum Phase {
        Send,
        Read { bytes: UpstreamBytes },
        Done,
    }
    stream::unfold(
        (
            Phase::Send,
            SseParser::default(),
            std::collections::VecDeque::<Result<ResponseEvent, Value>>::new(),
            context,
        ),
        move |(phase, parser, pending, context)| async move {
            let mut phase = phase;
            let mut parser = parser;
            let mut pending = pending;
            loop {
                match phase {
                    Phase::Send => match send_classified(&context).await {
                        SendClassified::Relay { bytes } => {
                            phase = Phase::Read { bytes };
                        }
                        SendClassified::Failed { envelope, .. } => {
                            return Some((Err(envelope), (Phase::Done, parser, pending, context)));
                        }
                    },
                    Phase::Read { bytes } => {
                        let mut bytes = bytes;
                        loop {
                            if let Some(item) = pending.pop_front() {
                                // A terminal item ends the stream: return to
                                // `Done` so a consumer that polls past the
                                // error cannot resume relaying upstream
                                // events behind it.
                                let next = if item.is_err() {
                                    Phase::Done
                                } else {
                                    Phase::Read { bytes }
                                };
                                return Some((item, (next, parser, pending, context)));
                            }
                            match bytes.as_mut().next().await {
                                Some(Ok(chunk)) => {
                                    let (events, malformed) = parser.push(&chunk);
                                    pending.extend(events.into_iter().map(Ok));
                                    if malformed {
                                        pending.push_back(Err(malformed_frame_envelope().await));
                                    }
                                }
                                Some(Err(error)) => {
                                    let envelope = adapter_error_envelope(transport_error(
                                        error.without_url().to_string(),
                                    ))
                                    .await;
                                    return Some((
                                        Err(envelope),
                                        (Phase::Done, parser, pending, context),
                                    ));
                                }
                                None => return None,
                            }
                        }
                    }
                    Phase::Done => return None,
                }
            }
        },
    )
}

/// Frame-buffer and parse the upstream SSE byte stream into
/// [`ResponseEvent`]s. A body error becomes an error envelope
/// (`transport_error`), so every producer failure renders as one terminal SSE
/// `error` event instead of an aborted stream. A complete frame whose data is
/// not valid JSON ends the stream the same way, after any events that preceded
/// it: the client must not receive a synthesized completion over corrupted
/// upstream data.
pub(super) fn parsed_events(
    bytes: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
) -> impl Stream<Item = Result<ResponseEvent, Value>> + Send + 'static {
    stream::unfold(
        (
            Box::pin(bytes),
            SseParser::default(),
            std::collections::VecDeque::<Result<ResponseEvent, Value>>::new(),
            false,
        ),
        |(mut bytes, mut parser, mut pending, mut done)| async move {
            loop {
                if done {
                    return None;
                }
                if let Some(item) = pending.pop_front() {
                    // A terminal item ends the stream: a consumer that polls
                    // past the error must not resume relaying upstream events
                    // behind it.
                    done = item.is_err();
                    return Some((item, (bytes, parser, pending, done)));
                }
                match bytes.next().await {
                    Some(Ok(chunk)) => {
                        let (events, malformed) = parser.push(&chunk);
                        pending.extend(events.into_iter().map(Ok));
                        if malformed {
                            pending.push_back(Err(malformed_frame_envelope().await));
                        }
                    }
                    Some(Err(error)) => {
                        let envelope = adapter_error_envelope(transport_error(
                            error.without_url().to_string(),
                        ))
                        .await;
                        return Some((Err(envelope), (bytes, parser, pending, true)));
                    }
                    None => return None,
                }
            }
        },
    )
}

/// The terminal envelope for an upstream frame whose `data` is present but not
/// valid JSON: a post-acceptance gateway failure (`own_error`, so the chain
/// never replays the turn) surfaced as one SSE `error` event.
async fn malformed_frame_envelope() -> Value {
    adapter_error_envelope(own_error(
        "upstream sent an SSE frame whose data is not valid JSON".to_string(),
    ))
    .await
}

/// Await a spawned tiktoken estimate under a wall-clock budget: the synthetic
/// `message_start` commits the response before any upstream byte and must not
/// wait on the estimator, so a saturated blocking pool or a pathological
/// input cannot delay the commit. `0` is a valid seed when the budget elapses
/// or the task failed.
pub(super) async fn bounded_input_estimate(
    handle: tokio::task::JoinHandle<u64>,
    budget: std::time::Duration,
) -> u64 {
    tokio::time::timeout(budget, handle)
        .await
        .unwrap_or_else(|_| Ok(0))
        .unwrap_or(0)
}

/// Translate parsed upstream events through the [`AnthropicSseMachine`] into
/// Anthropic SSE bytes. A producer error envelope becomes an SSE `error` event
/// and ends the stream; a producer that ends before a terminal event gets the
/// synthesized completion prefixed with the upstream-cut marker
/// (`stream_metrics::UPSTREAM_TRUNCATED_MARKER`), exactly like the
/// pre-early-commit relay.
pub(super) fn translated_stream(
    events: impl Stream<Item = Result<ResponseEvent, Value>> + Send + 'static,
    machine: AnthropicSseMachine,
) -> impl Stream<Item = Result<Bytes, Infallible>> + Send + 'static {
    translated_core(events, machine, |event, machine| {
        machine.apply(event).into_iter().collect::<String>()
    })
}

/// One item of a pooled streaming turn: the winning account's attribution
/// frame, or a relayed Responses event.
pub(super) enum PoolEvent {
    /// The account that won the turn, emitted once before its first relayed
    /// frame. Replaces the `x-shunt-account` response header on the
    /// early-commit path, where headers go out before the winner is known.
    Account(String),
    Event(ResponseEvent),
}

/// Translate pooled items into client-facing bytes: `Account` becomes an
/// `event: account` frame whose data is the bare account name (mirroring the
/// `x-shunt-account` header value), `Event` goes through the machine.
pub(super) fn pool_translated_stream(
    events: impl Stream<Item = Result<PoolEvent, Value>> + Send + 'static,
    machine: AnthropicSseMachine,
) -> impl Stream<Item = Result<Bytes, Infallible>> + Send + 'static {
    translated_core(events, machine, |item, machine| match item {
        PoolEvent::Account(name) => sse("account", &Value::String(name)),
        PoolEvent::Event(event) => machine.apply(event).into_iter().collect::<String>(),
    })
}

/// The shared translation loop behind [`translated_stream`] and
/// [`pool_translated_stream`]: a producer error envelope becomes an SSE
/// `error` event and ends the stream; a producer that ends before a terminal
/// event gets the synthesized completion prefixed with the upstream-cut
/// marker (`stream_metrics::UPSTREAM_TRUNCATED_MARKER`), exactly like the
/// pre-early-commit relay.
fn translated_core<I, F>(
    events: impl Stream<Item = Result<I, Value>> + Send + 'static,
    machine: AnthropicSseMachine,
    map: F,
) -> impl Stream<Item = Result<Bytes, Infallible>> + Send + 'static
where
    F: Fn(I, &mut AnthropicSseMachine) -> String + Send + 'static,
{
    stream::unfold(
        (Box::pin(events), machine, false, map),
        move |(mut events, mut machine, mut finished, map)| {
            async move {
                if finished {
                    return None;
                }
                loop {
                    match events.next().await {
                        Some(Ok(item)) => {
                            let data = map(item, &mut machine);
                            if !data.is_empty() {
                                return Some((
                                    Ok(Bytes::from(data)),
                                    (events, machine, false, map),
                                ));
                            }
                        }
                        Some(Err(envelope)) => {
                            return Some((
                                Ok(Bytes::from(sse("error", &envelope))),
                                (events, machine, true, map),
                            ));
                        }
                        None => {
                            let data = machine.finish().join("");
                            finished = true;
                            if data.is_empty() {
                                return None;
                            }
                            // `machine.finish()` only produced output here because
                            // the upstream connection ended before a real
                            // terminal/error event (see `AnthropicSseMachine::finish`):
                            // prefix the synthesized completion with an SSE comment
                            // marker so `stream_metrics::observe_response` can still
                            // classify this as an upstream cut instead of a normal
                            // completion. Real clients ignore `:`-prefixed comment
                            // lines per the WHATWG EventSource spec, so the
                            // client-visible stream stays exactly as well-formed as
                            // before.
                            let mut marked = Vec::with_capacity(
                                crate::stream_metrics::UPSTREAM_TRUNCATED_MARKER.len()
                                    + 2
                                    + data.len(),
                            );
                            marked.extend_from_slice(
                                crate::stream_metrics::UPSTREAM_TRUNCATED_MARKER,
                            );
                            marked.extend_from_slice(b"\n\n");
                            marked.extend_from_slice(data.as_bytes());
                            return Some((
                                Ok(Bytes::from(marked)),
                                (events, machine, finished, map),
                            ));
                        }
                    }
                }
            }
        },
    )
}

/// Frame-buffers the upstream SSE byte stream. Buffering raw bytes — rather than
/// decoding each transport chunk with `from_utf8_lossy` — keeps a multi-byte
/// UTF-8 code point intact when it straddles a chunk boundary: the incomplete
/// trailing bytes stay in the buffer until the next chunk completes them. Frame
/// boundaries are the ASCII `\n\n` or `\r\n\r\n` (the SSE spec permits CRLF
/// line endings, and `model_rewrite.rs` accepts both for the same reason);
/// neither can fall inside a multi-byte sequence, so every extracted frame is
/// already complete UTF-8. CRLF frames are normalized to LF before parsing.
#[derive(Default)]
struct SseParser {
    buffer: Vec<u8>,
    scan_from: usize,
}

impl SseParser {
    /// Feed one transport chunk. Returns every event the chunk completed, plus
    /// whether a complete frame carried data that is not valid JSON: the stream
    /// then ends with a terminal SSE `error` event instead of relaying a
    /// synthesized completion over a corrupted upstream.
    fn push(&mut self, chunk: &[u8]) -> (Vec<ResponseEvent>, bool) {
        self.buffer.extend_from_slice(chunk);

        let mut complete_end = None;
        let mut scan = self.scan_from;
        while scan < self.buffer.len() {
            if self.buffer[scan..].starts_with(b"\n\n") {
                complete_end = Some(scan + 2);
                scan += 2;
            } else if self.buffer[scan..].starts_with(b"\r\n\r\n") {
                complete_end = Some(scan + 4);
                scan += 4;
            } else {
                scan += 1;
            }
        }

        let Some(complete_end) = complete_end else {
            // The final bytes may be a prefix of a frame terminator, so scan
            // them again after the next chunk arrives. Everything before has
            // already been ruled out.
            self.scan_from = self.buffer.len().saturating_sub(3);
            return (Vec::new(), false);
        };

        // Parse all complete frames in one UTF-8 decode, then compact the buffer
        // once. Front-draining each frame shifts the same trailing bytes over and
        // over when one transport chunk contains many SSE events.
        let raw = String::from_utf8_lossy(&self.buffer[..complete_end]);
        let normalized = if raw.contains('\r') {
            std::borrow::Cow::Owned(raw.replace("\r\n", "\n"))
        } else {
            raw
        };
        let mut events = Vec::new();
        let mut malformed = false;
        for frame in normalized.split("\n\n") {
            match crate::model::responses::parse_sse_frame(frame) {
                None => {}
                Some(Ok(event)) => events.push(event),
                Some(Err(_)) => {
                    malformed = true;
                    break;
                }
            }
        }
        self.buffer.drain(..complete_end);
        self.scan_from = self.buffer.len().saturating_sub(3);
        (events, malformed)
    }
}

/// The default relay options for these tests: the `gpt-5.2-codex` model with
/// both protocol toggles off.
#[cfg(test)]
pub(super) fn relay_opts() -> super::context::RelayOptions {
    super::context::RelayOptions {
        model: "gpt-5.2-codex".to_string(),
        thinking_enabled: false,
        tool_search_native: false,
    }
}

#[cfg(test)]
pub(super) fn codex_route() -> Route {
    Route {
        provider: "codex".to_string(),
        adapter: crate::routing::AdapterKind::Responses,
        model: "gpt-5.2-codex".to_string(),
        upstream_model: "gpt-5.2-codex".to_string(),
        effort: None,
        service_tier: None,
    }
}

#[cfg(test)]
#[path = "early_stream_tests.rs"]
mod tests;
