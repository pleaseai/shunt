//! The Responses adapter's early-commit streaming machinery: commit the SSE
//! response with a synthetic `message_start` before any upstream byte, drive
//! the upstream feed (send, retry, parsing) inside the stream, and turn every
//! pre-stream failure into one terminal Anthropic SSE `error` event.

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

use super::error::{adapter_error_envelope, mapped_upstream_error, own_error, transport_error};
use super::http::http_send;
/// never replays the turn) surfaced as one SSE `error` event.
async fn malformed_frame_envelope() -> Value {
    adapter_error_envelope(own_error(
        "upstream sent an SSE frame whose data is not valid JSON".to_string(),
    ))
    .await
}

pub(super) use super::sse_parse::{
    bounded_input_estimate, parsed_events, pool_translated_stream, translated_core,
    translated_stream, PoolEvent, SseParser,
};

/// The streaming response for the early-commit transport: emit the synthetic
/// `message_start` + initial ping immediately — before any upstream byte — and
/// then relay the translated events. The keepalive wrapper spans both phases,
/// so the client hop is never silent for longer than `keepalive` even while
/// the upstream thinks in silence.
pub(super) fn early_streaming_response(
    machine: impl FnOnce() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = (AnthropicSseMachine, String)> + Send>,
        > + Send
        + 'static,
    keepalive: std::time::Duration,
    events: impl Stream<Item = Result<ResponseEvent, Value>> + Send + 'static,
) -> axum::response::Response {
    let output = translated_core(events, machine, |event, machine| {
        machine.apply(event).into_iter().collect::<String>()
    });
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
    machine: impl FnOnce() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = (AnthropicSseMachine, String)> + Send>,
        > + Send
        + 'static,
    keepalive: std::time::Duration,
    events: impl Stream<Item = Result<PoolEvent, Value>> + Send + 'static,
) -> axum::response::Response {
    let output = translated_core(events, machine, |item, machine| match item {
        PoolEvent::Account(name) => sse("account", &Value::String(name)),
        PoolEvent::Event(event) => machine.apply(event).into_iter().collect::<String>(),
    });
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
    Relay {
        bytes: UpstreamBytes,
    },
    Failed {
        envelope: Value,
        status: StatusCode,
        /// Whether this failure is a relayed upstream status (eligible for
        /// the failover chain's remembered best-failure) or a gateway-
        /// synthesized transport error (advances without remembering, like
        /// the pre-commit loop's `BeforeHeaders`).
        remember: bool,
        /// Whether the chain should advance. A TTFB timeout is terminal: the
        /// configured timeout is an answer (`504 timeout_error`), not a
        /// transport failure, exactly like the pre-commit loop's mapping.
        advance: bool,
    },
}

/// Drive one bounded-retry send and classify the raw outcome. A retry-exhausted
/// transport error becomes a gateway error envelope with `502` (the pre-commit
/// path's shape); a non-2xx upstream status becomes the mapped envelope for
/// that status. The caller decides whether a failure advances a chain.
pub(super) async fn send_classified(context: &HttpSendContext) -> SendClassified {
    let body = super::body::prepare_body(
        &context.state,
        &context.route,
        context.upstream_body.as_ref(),
    )
    .await;
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
                body.clone(),
            )
        },
    )
    .await;
    let upstream = match outcome {
        Ok(response) => response,
        Err(error) => {
            let status = if matches!(error, crate::upstream_timeout::SendError::Timeout) {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_GATEWAY
            };
            let advance = !matches!(error, crate::upstream_timeout::SendError::Timeout);
            let envelope = adapter_error_envelope(
                error.into_adapter_error(|error| transport_error(error.without_url().to_string())),
            )
            .await;
            return SendClassified::Failed {
                envelope,
                status,
                remember: false,
                advance,
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
        return SendClassified::Failed {
            envelope,
            status,
            remember: true,
            advance: crate::proxy::failover::is_advance_status(status),
        };
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
    /// The raw translated request; prepared (zstd admission + blocking-pool
    /// work) at send time inside the committed stream, so compression load
    /// cannot delay the committed response.
    pub(super) upstream_body: std::sync::Arc<Value>,
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
