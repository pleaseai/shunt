//! The Responses adapter's early-commit streaming machinery: commit the SSE
//! response with a synthetic `message_start` before any upstream byte, drive
//! the upstream feed (send, retry, parsing) inside the stream, and turn every
//! pre-stream failure into one terminal Anthropic SSE `error` event.

use axum::{
    body::{Body, Bytes},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::{stream, Stream};
use serde_json::Value;

use crate::{
    auth::Credential,
    config::AuthMode,
    model::responses::{AnthropicSseMachine, ResponseEvent},
    routing::Route,
    server::AppState,
};

use super::context::CredentialSource;
use super::error::{adapter_error_envelope, mapped_upstream_error, own_error, transport_error};
use super::http::http_send;
use crate::proxy::chain_stream::LazyEnvelope;

pub(super) use super::sse_parse::{
    bounded_input_estimate, next_parsed, parsed_events, pool_translated_stream, pooled_first_poll,
    translated_core, translated_stream, MachineBuild, PoolEvent, PoolFirstPoll, PoolItem,
    SseParser,
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
    events: impl Stream<Item = Result<PoolItem, Value>> + Send + 'static,
) -> axum::response::Response {
    let output = pool_translated_stream(events, machine);
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

/// The shared machine factory for the committed single-route and pool
/// responses: the bounded estimate wait happens inside the committed stream
/// (keepalive pings cover it, the commit itself never stalls on the blocking
/// pool), then the synthetic start.
pub(super) fn estimated_machine_factory(
    turn: super::context::TurnOptions,
    route: Route,
    estimate_input: Option<std::sync::Arc<Value>>,
) -> impl FnOnce() -> std::pin::Pin<
    Box<dyn std::future::Future<Output = (AnthropicSseMachine, String)> + Send>,
> + Send {
    move || {
        Box::pin(async move {
            let input_tokens_estimate = match estimate_input {
                Some(request) => {
                    let handle = tokio::task::spawn_blocking(move || {
                        crate::count_tokens::count_input_tokens_value(&request)
                    });
                    bounded_input_estimate(handle, std::time::Duration::from_secs(1)).await
                }
                None => 0,
            };
            let mut machine = turn
                .relay(&route)
                .machine()
                .with_input_estimate(input_tokens_estimate)
                .without_content_accumulation();
            let start = machine.start_synthetic(format!("msg_{}", uuid::Uuid::new_v4()));
            (machine, start.join(""))
        })
    }
}

/// The raw outcome of one bounded-retry send, before the status is judged.
/// Split out of [`http_events_stream`] so the multi-upstream chain
/// (`proxy::chain_stream`) can classify a failure as advance-worthy without
/// duplicating the send, the quota capture, or the envelope building.
pub(super) enum SendClassified {
    Relay {
        bytes: UpstreamBytes,
    },
    Failed {
        /// `Deferred` for an advance-worthy relayed status, whose upstream
        /// body stays unread until the chain selects the failure as the
        /// terminal remembered best (the pre-commit loop's lazy-body rule);
        /// `Ready` for every terminal answer.
        envelope: LazyEnvelope,
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
    // The credential resolves before this call — the chain resolves it per
    // attempt, the single-route stream resolves it inside the committed
    // stream. Fail closed with a gateway error rather than panicking on the
    // invariant.
    let Some(credential) = context.credential.clone() else {
        let envelope = adapter_error_envelope(own_error(
            "responses credential was not resolved before the upstream send".to_string(),
        ))
        .await;
        return SendClassified::Failed {
            envelope: LazyEnvelope::Ready(envelope),
            status: StatusCode::BAD_GATEWAY,
            remember: false,
            advance: false,
        };
    };
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
                credential.clone(),
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
            let envelope =
                LazyEnvelope::Ready(
                    adapter_error_envelope(error.into_adapter_error(|error| {
                        transport_error(error.without_url().to_string())
                    }))
                    .await,
                );
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
        let envelope = if crate::proxy::failover::is_advance_status(status) {
            // Advance on the status alone: the body stays unread until the
            // chain selects this failure as the remembered best — a slow or
            // non-terminating error body must never block the fallback (the
            // pre-commit loop drops superseded responses unread).
            let auth = context.auth;
            LazyEnvelope::Deferred(Box::pin(async move {
                adapter_error_envelope(mapped_upstream_error(status, upstream, auth).await).await
            }))
        } else {
            LazyEnvelope::Ready(
                adapter_error_envelope(mapped_upstream_error(status, upstream, context.auth).await)
                    .await,
            )
        };
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

/// A non-pooled chain attempt's route-gated token estimate, raced against its
/// send ([`send_classified_with_estimate`]): resolved when the estimate won
/// the race, still pending when the send won it.
pub(super) enum EstimateBuild<'a> {
    Ready(u64),
    Pending(std::pin::Pin<Box<dyn std::future::Future<Output = u64> + Send + 'a>>),
}

/// One non-pooled chain attempt's send raced against its estimate.
/// `headers_at` is the send's completion instant, for the chain's
/// header-latency sample — never an estimate-completion instant.
pub(super) struct SendClassifiedWithEstimate<'a> {
    pub(super) classified: SendClassified,
    pub(super) estimate: EstimateBuild<'a>,
    pub(super) headers_at: std::time::Instant,
}

/// Race a non-pooled chain attempt's route-gated token estimate against its
/// bounded-retry send so the synthetic `message_start` seed no longer waits
/// for the estimate on top of the upstream round-trip — the non-pooled
/// counterpart of [`pooled_first_poll`]. Only the winner arm consumes the
/// estimate, so a classified failure never waits on it: a still-running
/// estimate is dropped with the failure, its blocking task completing
/// unobserved.
pub(super) async fn send_classified_with_estimate<'a>(
    send: impl std::future::Future<Output = SendClassified> + Send + 'a,
    estimate: impl std::future::Future<Output = u64> + Send + 'a,
) -> SendClassifiedWithEstimate<'a> {
    match futures_util::future::select(Box::pin(send), Box::pin(estimate)).await {
        futures_util::future::Either::Left((classified, estimate)) => SendClassifiedWithEstimate {
            classified,
            estimate: EstimateBuild::Pending(Box::pin(estimate)),
            headers_at: std::time::Instant::now(),
        },
        futures_util::future::Either::Right((value, send)) => {
            let classified = send.await;
            SendClassifiedWithEstimate {
                classified,
                estimate: EstimateBuild::Ready(value),
                headers_at: std::time::Instant::now(),
            }
        }
    }
}

/// Everything [`http_events_stream`] needs to drive one upstream send.
#[derive(Clone)]
pub(super) struct HttpSendContext {
    pub(super) state: AppState,
    pub(super) route: Route,
    pub(super) policy: crate::retry::RetryPolicy,
    /// Resolved before [`send_classified`]: the chain resolves it per
    /// attempt, the single-route stream resolves it inside the committed
    /// stream before the send.
    pub(super) credential: Option<Credential>,
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
    credential: CredentialSource,
    codex_quota_account: Option<crate::config::AccountConfig>,
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
            Some(credential),
            codex_quota_account,
        ),
        move |(phase, parser, pending, context, credential, quota_account)| async move {
            let mut phase = phase;
            let mut parser = parser;
            let mut pending = pending;
            let mut context = context;
            let mut credential = credential;
            let mut quota_account = quota_account;
            loop {
                match phase {
                    Phase::Send => {
                        // Resolve the credential inside the committed stream:
                        // a refreshable credential's refresh is outside the
                        // TTFB timeout, and keepalive pings cover the wait.
                        if let Some(source) = credential.take() {
                            let resolved = match source {
                                CredentialSource::Resolved(credential) => credential,
                                CredentialSource::Deferred(resolve) => match resolve.await {
                                    Ok(credential) => credential,
                                    Err(error) => {
                                        let envelope = adapter_error_envelope(error).await;
                                        return Some((
                                            Err(envelope),
                                            (
                                                Phase::Done,
                                                parser,
                                                pending,
                                                context,
                                                credential,
                                                quota_account,
                                            ),
                                        ));
                                    }
                                },
                            };
                            if quota_account.is_none() {
                                quota_account = super::codex_quota_account(&resolved);
                            }
                            context.credential = Some(resolved);
                            context.codex_quota_account = quota_account.take();
                        }
                        match send_classified(&context).await {
                            SendClassified::Relay { bytes } => {
                                phase = Phase::Read { bytes };
                            }
                            SendClassified::Failed { envelope, .. } => {
                                let envelope = envelope.resolve().await;
                                return Some((
                                    Err(envelope),
                                    (
                                        Phase::Done,
                                        parser,
                                        pending,
                                        context,
                                        credential,
                                        quota_account,
                                    ),
                                ));
                            }
                        }
                    }
                    Phase::Read { bytes } => {
                        let mut bytes = bytes;
                        match next_parsed(&mut parser, &mut pending, &mut bytes).await {
                            Some(item) => {
                                // A terminal item ends the stream: return to
                                // `Done` so a consumer that polls past the
                                // error cannot resume relaying upstream
                                // events behind it.
                                let next = if item.is_err() {
                                    Phase::Done
                                } else {
                                    Phase::Read { bytes }
                                };
                                return Some((
                                    item,
                                    (next, parser, pending, context, credential, quota_account),
                                ));
                            }
                            None => return None,
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
