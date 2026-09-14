//! Multi-upstream streaming chains: run the failover loop inside the committed
//! SSE response.
//!
//! A streaming turn with ordered upstreams used to bypass the failover chain:
//! the Responses adapter committed `200` with a synthetic `message_start`
//! before the first upstream attempt, so any pre-header failure became a
//! terminal SSE error instead of advancing the chain. This module drives the
//! chain *inside* the committed stream instead: the response still commits
//! immediately (keepalive pings cover the wait), but the synthetic
//! `message_start` is deferred until an upstream wins, and pre-header failures
//! advance to the next upstream. An Anthropic-kind winner relays its own SSE
//! (`message_start` included), so the client sees exactly one start either way.
//!
//! The chain covers `Anthropic`- and `Responses`-kind routes without the
//! websocket transport; any other combination keeps the pre-commit loop (see
//! `docs/upstreams-failover.md` §6 for the remaining deviation).

use std::convert::Infallible;
use std::pin::Pin;
use std::time::{Duration, Instant};

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
};
use futures_util::{stream, Stream, StreamExt};
use serde_json::Value;

use crate::{
    error::ShuntError,
    model::responses::sse,
    observability,
    request::RequestBody,
    routing::{AdapterKind, Route},
    server::AppState,
    stream_metrics::{self, Protocol},
};

use super::failover::{headers_for_route, InboundContext};
use super::ForwardError;

/// Whether this request is one this module drives: more than one upstream, a
/// streaming client, at least one Responses-kind route (the only kind that
/// early-commits), and only `Anthropic`/`Responses` kinds without the
/// websocket transport.
pub(super) fn chain_stream_applies(state: &AppState, routes: &[Route], body: &RequestBody) -> bool {
    routes.len() > 1
        && body
            .json()
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        && routes
            .iter()
            .any(|route| route.adapter == AdapterKind::Responses)
        && routes.iter().all(|route| {
            matches!(
                route.adapter,
                AdapterKind::Responses | AdapterKind::Anthropic
            ) && !(route.adapter == AdapterKind::Responses
                && state.config.codex_websocket_enabled(&route.provider))
        })
}

/// One upstream attempt's outcome, produced inside the committed stream.
pub(crate) enum Attempt {
    /// This upstream won. `start` carries the synthetic `message_start` bytes
    /// for a Responses-kind winner (deferred until now); an Anthropic-kind
    /// winner has none — its own `message_start` relays as the first frame.
    Winner {
        start: Option<Bytes>,
        frames: ClientFrames,
    },
    /// The attempt failed before any client-visible frame. `advance` mirrors
    /// the pre-commit loop: advance-status and transport failures move the
    /// chain on, anything else is terminal.
    Failed {
        advance: bool,
        envelope: Value,
        status: StatusCode,
    },
}

pub(crate) type ClientFrames = Pin<Box<dyn Stream<Item = Result<Bytes, Infallible>> + Send>>;

/// Everything the committed-stream chain needs, bundled so the per-attempt
/// closure carries one context instead of nine arguments.
pub(super) struct ChainStreamRequest {
    pub(super) state: AppState,
    pub(super) routes: Vec<Route>,
    pub(super) uri: Uri,
    pub(super) base_headers: HeaderMap,
    pub(super) inbound: InboundContext,
    pub(super) primary_origin: Option<String>,
    pub(super) body: RequestBody,
    pub(super) requested_model: String,
    pub(super) started_at: Instant,
}

/// Drive the failover chain for a streaming turn and return the committed SSE
/// response. The caller has already verified [`chain_stream_applies`].
pub(super) async fn forward_chain_stream(
    request: ChainStreamRequest,
) -> Result<(StatusCode, axum::response::Response), ForwardError> {
    let ChainStreamRequest {
        state,
        routes,
        uri,
        base_headers,
        inbound,
        primary_origin,
        body,
        requested_model,
        started_at,
    } = request;
    let first_route = routes
        .first()
        .expect("route chains are non-empty after resolution");
    let first_provider = first_route.provider.clone();
    let first_model = first_route.model.clone();
    let first_upstream_model = first_route.upstream_model.clone();
    // The synthetic start's input-token estimate counts the client's request,
    // which is identical for every attempt; encode once up front (bounded, so a
    // slow blocking pool cannot delay the committed response) and reuse it for
    // whichever upstream wins.
    let estimate = estimate_input_tokens(&state, &routes, &body).await;

    enum Phase {
        Attempt {
            attempts: std::collections::VecDeque<(usize, Route)>,
            remembered: Option<Remembered>,
        },
        Relay {
            frames: ClientFrames,
        },
        Done,
    }

    struct Remembered {
        envelope: Value,
    }

    let attempts: std::collections::VecDeque<(usize, Route)> =
        routes.into_iter().enumerate().collect();
    let attempts_len = attempts.len();
    let stream_state = state.clone();
    let stream_uri = uri.clone();
    let stream_headers = base_headers.clone();
    let output = stream::unfold(
        (
            Phase::Attempt {
                attempts,
                remembered: None,
            },
            body,
            first_provider.clone(),
            first_model.clone(),
        ),
        move |(mut phase, body, span_provider, span_model)| {
            let state = stream_state.clone();
            let uri = stream_uri.clone();
            let base_headers = stream_headers.clone();
            let inbound = inbound.clone();
            let primary_origin = primary_origin.clone();
            async move {
                loop {
                    match phase {
                        Phase::Relay { mut frames } => match frames.next().await {
                            Some(Ok(bytes)) => {
                                return Some((
                                    Ok::<Bytes, Infallible>(bytes),
                                    (Phase::Relay { frames }, body, span_provider, span_model),
                                ));
                            }
                            Some(Err(_)) | None => {
                                observability::record_span_outcome(&span_provider, StatusCode::OK);
                                observability::capture_upstream_outcome(
                                    &span_provider,
                                    &span_model,
                                    StatusCode::OK,
                                );
                                return Some((
                                    Ok(Bytes::new()),
                                    (Phase::Done, body, span_provider, span_model),
                                ));
                            }
                        },
                        Phase::Done => return None,
                        Phase::Attempt {
                            mut attempts,
                            mut remembered,
                        } => {
                            let Some((index, route)) = attempts.pop_front() else {
                                let envelope =
                                    match remembered.map(|remembered| remembered.envelope) {
                                        Some(envelope) => envelope,
                                        None => {
                                            crate::error::error_body_value(
                                                ShuntError::new(
                                                    StatusCode::BAD_GATEWAY,
                                                    "api_error",
                                                    format!(
                                                "all upstreams failed ({attempts_len} attempted)"
                                            ),
                                                )
                                                .into_response(),
                                            )
                                            .await
                                        }
                                    };
                                let frame = sse("error", &envelope);
                                observability::record_span_outcome(
                                    &span_provider,
                                    StatusCode::BAD_GATEWAY,
                                );
                                observability::capture_upstream_outcome(
                                    &span_provider,
                                    &span_model,
                                    StatusCode::BAD_GATEWAY,
                                );
                                return Some((
                                    Ok(Bytes::from(frame)),
                                    (Phase::Done, body, span_provider, span_model),
                                ));
                            };
                            crate::metrics::record_failover(&route.provider, "attempted");
                            let attempt_started = Instant::now();
                            let attempt_headers = headers_for_route(
                                &state,
                                &route,
                                &base_headers,
                                &inbound,
                                index == 0,
                                primary_origin.as_deref(),
                            );
                            let attempt_body = body.clone();
                            let provider = route.provider.clone();
                            let model = route.model.clone();
                            let outcome = match route.adapter {
                                AdapterKind::Responses => {
                                    crate::adapters::responses::chain_attempt(
                                        &state,
                                        &route,
                                        &attempt_headers,
                                        attempt_body,
                                        estimate,
                                    )
                                    .await
                                }
                                AdapterKind::Anthropic => {
                                    crate::adapters::anthropic::chain_attempt(
                                        &state,
                                        &route,
                                        &uri,
                                        &attempt_headers,
                                        attempt_body,
                                    )
                                    .await
                                }
                                _ => unreachable!("chain_stream_applies gates the adapter kind"),
                            };
                            match outcome {
                                Attempt::Winner { start, frames } => {
                                    crate::metrics::record_proxied_request(
                                        &provider,
                                        &model,
                                        StatusCode::OK.as_u16(),
                                        attempt_started.elapsed().as_secs_f64() * 1000.0,
                                    );
                                    match start {
                                        Some(start) => {
                                            return Some((
                                                Ok(start),
                                                (
                                                    Phase::Relay { frames },
                                                    body,
                                                    span_provider,
                                                    span_model,
                                                ),
                                            ));
                                        }
                                        None => {
                                            phase = Phase::Relay { frames };
                                            continue;
                                        }
                                    }
                                }
                                Attempt::Failed {
                                    advance,
                                    envelope,
                                    status,
                                } => {
                                    crate::metrics::record_proxied_request(
                                        &provider,
                                        &model,
                                        status.as_u16(),
                                        attempt_started.elapsed().as_secs_f64() * 1000.0,
                                    );
                                    if advance && !attempts.is_empty() {
                                        crate::metrics::record_failover(
                                            &route.provider,
                                            "advanced",
                                        );
                                        tracing::warn!(
                                            provider = %provider,
                                            model = %model,
                                            status = status.as_u16(),
                                            "upstream failed before the first relayed frame; advancing the committed streaming chain"
                                        );
                                        remembered = Some(Remembered { envelope });
                                        phase = Phase::Attempt {
                                            attempts,
                                            remembered,
                                        };
                                        continue;
                                    }
                                    let frame = sse("error", &envelope);
                                    observability::record_span_outcome(&span_provider, status);
                                    observability::capture_upstream_outcome(
                                        &span_provider,
                                        &span_model,
                                        status,
                                    );
                                    return Some((
                                        Ok(Bytes::from(frame)),
                                        (Phase::Done, body, span_provider, span_model),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        },
    );

    let response = axum::response::Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(crate::keepalive::with_pings(
            output,
            Duration::from_secs(state.config.server.sse_keepalive_seconds),
        )))
        .expect("response builder uses valid status and headers")
        .into_response();
    let mut response = stream_metrics::observe_response(
        response,
        Protocol::Anthropic,
        first_provider.clone(),
        first_model.clone(),
        started_at,
    );
    super::failover::stamp_gateway_headers(
        &mut response,
        &first_provider,
        &requested_model,
        &first_upstream_model,
    );
    Ok((StatusCode::OK, response))
}

/// The local tiktoken estimate for the synthetic `message_start`, when any
/// chain route's provider opts into local counting. Bounded so a saturated
/// blocking pool cannot delay the committed response; a missed deadline yields
/// `0`, exactly like the single-route path's bounded await.
async fn estimate_input_tokens(state: &AppState, routes: &[Route], body: &RequestBody) -> u64 {
    let counts_locally = routes.iter().any(|route| {
        state
            .config
            .provider(&route.provider)
            .map(|provider| provider.count_tokens == crate::config::CountTokens::Tiktoken)
            .unwrap_or(false)
    });
    if !counts_locally {
        return 0;
    }
    let request = body.json_arc();
    let handle = tokio::task::spawn_blocking(move || {
        crate::count_tokens::count_input_tokens_value(&request)
    });
    tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or(0)
}
