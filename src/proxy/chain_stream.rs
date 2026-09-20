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
use std::future::Future;
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
use crate::adapters::responses::spawn_terminal_drain;

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
                // The Anthropic account pools (claude_oauth / kimi_oauth)
                // resolve their credentials per-account inside their own loop,
                // which the in-stream attempt does not (yet) drive; those
                // chains keep the pre-commit loop, which handles them.
                && !(route.adapter == AdapterKind::Anthropic
                    && matches!(
                        state
                            .config
                            .provider(&route.provider)
                            .map(|provider| provider.auth),
                        Some(
                            crate::config::AuthMode::ClaudeOauth
                                | crate::config::AuthMode::KimiOauth
                        )
                    ))
        })
}

/// An error envelope that is either ready or built on demand. A relayed
/// status carries its upstream response with the body unread — the pre-commit
/// loop's lazy-body rule, where superseded failures drop unread — and the
/// chain resolves the envelope only once that failure is selected as the
/// terminal answer (a terminal status's budgeted read runs right before its
/// error frame, after the latency sample).
pub(crate) enum LazyEnvelope {
    Ready(Value),
    Deferred(Pin<Box<dyn Future<Output = Value> + Send>>),
}

impl LazyEnvelope {
    pub(crate) async fn resolve(self) -> Value {
        match self {
            Self::Ready(value) => value,
            Self::Deferred(build) => build.await,
        }
    }
}

/// A winner's relay build: ready when the machine resolved before the
/// headers (or the route never estimates — an Anthropic-kind winner relays
/// its own `message_start` and has no synthetic start), pending when the
/// headers won the race. The pending build resolves only the machine and
/// the synthetic `message_start`, awaited by the chain AFTER it recorded
/// the winner — a client disconnect during that await drops the unfold,
/// and the attribution (slots, requests sample, span outcome) must already
/// name the winner, not the failed primary.
pub(crate) enum RelayBuild {
    Ready {
        start: Option<Bytes>,
        frames: ClientFrames,
    },
    Pending(Pin<Box<dyn Future<Output = (Bytes, ClientFrames)> + Send>>),
}

/// One upstream attempt's outcome, produced inside the committed stream.
pub(crate) enum Attempt {
    /// This upstream won. `headers_at` is when the winning upstream's
    /// response headers arrived: the chain records `shunt.latency` to that
    /// instant (the pre-commit loop's semantics), so a still-running
    /// estimate awaited after the headers never inflates the
    /// header-latency sample.
    Winner {
        headers_at: Instant,
        relay: RelayBuild,
    },
    /// The attempt failed before any client-visible frame. `advance` mirrors
    /// the pre-commit loop: advance-status and transport failures move the
    /// chain on, anything else is terminal. `remember` mirrors the loop's
    /// remembered-failure eligibility: a relayed upstream status is eligible
    /// for the best-failure preference, a gateway-synthesized transport error
    /// advances without being remembered.
    Failed {
        advance: bool,
        remember: bool,
        envelope: LazyEnvelope,
        status: StatusCode,
    },
}

/// A winner's relay: `Ok` bytes flow to the client; `Err` carries the error
/// envelope for one terminal SSE `error` event (a mid-relay body failure, so
/// the client sees the failure instead of a silently truncated stream and the
/// outcome records as failed rather than `OK`). A body error after the turn's
/// terminal frame already relayed can no longer reach the relay: it ends at
/// that frame, so the post-terminal drain absorbs whatever follows.
pub(crate) type ClientFrames = Pin<Box<dyn Stream<Item = Result<Bytes, Value>> + Send>>;

/// Cross-chunk scanner over a winner's relayed frames: once a complete
/// terminal frame — `message_stop`, or a relayed `error` frame (an
/// Anthropic-kind upstream's own mid-stream error, or a translated backend
/// error event) — has passed, the relay ends at that frame — the Responses
/// relay's post-terminal rule (`adapters::responses::sse_parse`) — and a
/// later transport error surfaces nowhere: the client holds a complete turn,
/// and an appended error event would both corrupt it and (the observer gives
/// error events precedence over terminal events) record the request as
/// failed.
struct TerminalScan {
    /// Bytes since the last complete frame boundary, held until the next
    /// boundary arrives (a frame may straddle chunk boundaries).
    carry: Vec<u8>,
    terminal_seen: bool,
    /// Set while the pending frame exceeds the terminal bound: its bytes are
    /// not buffered (no terminal frame is that large) and scanning resumes
    /// once its boundary passes — the stream-metrics observer's skip.
    skipping_oversized: bool,
    /// The skipped frame's last bytes, so a boundary split across relay
    /// chunks still ends the skip.
    skip_tail: [u8; 4],
    skip_tail_len: usize,
}

/// No relayed frame can be terminal past this size (real frames are a few
/// hundred bytes); the bound mirrors the model-rewrite accumulator's.
const MAX_TERMINAL_FRAME_BYTES: usize = 64 * 1024;

impl TerminalScan {
    fn new() -> Self {
        Self {
            carry: Vec::new(),
            terminal_seen: false,
            skipping_oversized: false,
            skip_tail: [0; 4],
            skip_tail_len: 0,
        }
    }

    /// Feed one relayed chunk. When the terminal frame completes inside this
    /// chunk, returns the offset just past its boundary within the chunk —
    /// earlier bytes of the frame may have arrived in previous chunks — so
    /// the relay cuts there and forwards nothing past the terminal;
    /// otherwise `None`.
    fn scan(&mut self, chunk: &[u8]) -> Option<usize> {
        if self.terminal_seen {
            return None;
        }
        // Bytes of the caller's chunk consumed by an in-progress skip; they
        // belong to the oversized frame, whose content is not scanned.
        let skipped = if self.skipping_oversized {
            let consumed = self.skip_to_boundary(chunk);
            if self.skipping_oversized {
                return None;
            }
            consumed
        } else {
            0
        };
        let rest = &chunk[skipped..];
        let carried = self.carry.len();
        self.carry.extend_from_slice(rest);
        let mut consumed = 0;
        while let Some(end) = sse_frame_boundary(&self.carry[consumed..]) {
            if is_terminal_frame(&self.carry[consumed..consumed + end]) {
                self.terminal_seen = true;
                self.carry.clear();
                return Some(skipped + consumed + end - carried);
            }
            consumed += end;
        }
        self.carry.copy_within(consumed.., 0);
        self.carry.truncate(self.carry.len() - consumed);
        if self.carry.len() > MAX_TERMINAL_FRAME_BYTES {
            self.begin_oversized_skip();
        }
        None
    }

    /// The pending frame cannot be a terminal: drop the buffered prefix and
    /// skip the frame's remaining bytes, keeping its last bytes so a
    /// boundary split across relay chunks still ends the skip. Mirrors the
    /// stream-metrics observer's skip.
    fn begin_oversized_skip(&mut self) {
        let retained = self.carry.len().min(self.skip_tail.len());
        self.skip_tail_len = retained;
        self.skip_tail[..retained].copy_from_slice(&self.carry[self.carry.len() - retained..]);
        self.carry.clear();
        self.skipping_oversized = true;
    }

    /// Consume bytes through the oversized frame's first boundary. Only used
    /// after a frame has exceeded the terminal bound, never on the hot path.
    fn skip_to_boundary(&mut self, bytes: &[u8]) -> usize {
        for (index, &byte) in bytes.iter().enumerate() {
            if self.skip_tail_len < self.skip_tail.len() {
                self.skip_tail[self.skip_tail_len] = byte;
                self.skip_tail_len += 1;
            } else {
                self.skip_tail.copy_within(1.., 0);
                self.skip_tail[self.skip_tail.len() - 1] = byte;
            }
            let tail = &self.skip_tail[..self.skip_tail_len];
            if tail.ends_with(b"\n\n") || tail.ends_with(b"\r\n\r\n") {
                self.skipping_oversized = false;
                self.skip_tail_len = 0;
                return index + 1;
            }
        }
        bytes.len()
    }
}

/// Byte index just past the first SSE frame boundary (`\n\n` or `\r\n\r\n`),
/// whichever appears first, or `None` if the buffer holds no complete frame.
fn sse_frame_boundary(buf: &[u8]) -> Option<usize> {
    let lf = buf
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|p| p + 2);
    let crlf = buf
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|p| p + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Whether the complete frame carries a terminal event — `message_stop`, or
/// a relayed `error` frame (an Anthropic-kind upstream's own mid-stream
/// error, or a translated backend error event). The `event:` field parses
/// through the metrics observer's own parser, so the scan and the observer
/// cannot disagree about a frame's event name; only a frame's own `event:`
/// line matches — a `data:` payload whose JSON happens to contain the
/// literal must not arm the scan.
fn is_terminal_frame(frame: &[u8]) -> bool {
    let Some(event) = stream_metrics::event_and_data(frame).0 else {
        return false;
    };
    event == b"message_stop" || event == b"error"
}

/// Resolve a winner's relay build into its start and frames, recording the
/// winner FIRST: the attribution slots, the requests sample, and the span
/// outcome all land before a still-pending build is awaited — a client
/// disconnect during that await drops the unfold, and the records must
/// already name the winner, not the failed primary. The pending build
/// resolves only the machine and the synthetic start; the keepalive wrapper
/// covers the wait.
async fn resolve_winner(
    provider: &str,
    model: &str,
    header_latency_ms: f64,
    request_span: &tracing::Span,
    winner_slot: &std::sync::Arc<std::sync::Mutex<String>>,
    winner_model_slot: &std::sync::Arc<std::sync::Mutex<String>>,
    relay: RelayBuild,
) -> (Option<Bytes>, ClientFrames) {
    crate::metrics::record_proxied_request(
        provider,
        model,
        StatusCode::OK.as_u16(),
        header_latency_ms,
    );
    // Attribute before the start goes out: TTFT and early-stream metrics
    // must name the winner, not the failed primary.
    if let Ok(mut slot) = winner_slot.lock() {
        *slot = provider.to_string();
    }
    if let Ok(mut slot) = winner_model_slot.lock() {
        *slot = model.to_string();
    }
    // Record the winner at selection: a client disconnect mid-relay drops
    // the unfold, and the request span must already carry the winner and
    // its 200. A later mid-relay failure overwrites only the span status;
    // its Sentry event is the stream observer's.
    observability::record_span_outcome_on(request_span, provider, StatusCode::OK);
    observability::capture_upstream_outcome(provider, model, StatusCode::OK);
    match relay {
        RelayBuild::Ready { start, frames } => (start, frames),
        RelayBuild::Pending(build) => {
            let (start, frames) = build.await;
            (Some(start), frames)
        }
    }
}

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
    /// What a `[models.router]` entry decided, if the requested id carries one.
    /// Known before any upstream is contacted, so it is stamped on this path
    /// even though the upstream-naming headers cannot be.
    pub(super) router_stamp: Option<super::failover::OwnedRouterStamp>,
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
        router_stamp,
    } = request;
    // Read before the body moves into the chain: the committed stream's
    // `message_delta` carries the auto-mode classifier's answer, which only a
    // first-party relay produces (see `crate::proxy::safeguards`).
    let requested_safeguards = crate::proxy::safeguards::requested_types(body.json());
    let max_request_bytes = state.config.server.limits.max_request_bytes;
    let first_route = routes
        .first()
        .expect("route chains are non-empty after resolution");
    let first_provider = first_route.provider.clone();
    let first_model = first_route.model.clone();
    // Captured synchronously inside the caller's `.instrument(span)` future,
    // so the eventual in-stream outcome records land on the request's own
    // span rather than whatever span is current while the body is polled.
    let request_span = tracing::Span::current();
    let last_provider = routes
        .last()
        .expect("route chains are non-empty after resolution")
        .provider
        .clone();
    let last_model = routes
        .last()
        .expect("route chains are non-empty after resolution")
        .model
        .clone();
    enum Phase {
        Attempt {
            attempts: std::collections::VecDeque<(usize, Route)>,
            remembered: Option<Remembered>,
        },
        Relay {
            frames: ClientFrames,
            winner_provider: String,
            winner_model: String,
            terminal: TerminalScan,
        },
        Done,
    }

    struct Remembered {
        envelope: LazyEnvelope,
        status: StatusCode,
        provider: String,
        model: String,
    }

    // Created before the unfold so the closure can capture clones and write
    // the winner into them once the stream knows one.
    let winner_slot = std::sync::Arc::new(std::sync::Mutex::new(first_provider.clone()));
    let closure_slot = winner_slot.clone();
    let winner_model_slot = std::sync::Arc::new(std::sync::Mutex::new(first_model.clone()));
    let closure_model_slot = winner_model_slot.clone();
    // One chain, one token estimate: the first opted-in attempt starts the
    // bounded blocking encode and later opted-in attempts reuse the same
    // compute (see `ChainEstimate`) instead of re-tokenizing the identical
    // body per attempt.
    let estimate_cache = std::sync::Arc::new(crate::adapters::responses::ChainEstimate::default());
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
            Some(body),
        ),
        move |(mut phase, body)| {
            let state = stream_state.clone();
            let uri = stream_uri.clone();
            let base_headers = stream_headers.clone();
            let inbound = inbound.clone();
            let primary_origin = primary_origin.clone();
            let last_provider = last_provider.clone();
            let last_model = last_model.clone();
            let request_span = request_span.clone();
            let winner_slot = closure_slot.clone();
            let winner_model_slot = closure_model_slot.clone();
            let estimate_cache = estimate_cache.clone();
            async move {
                let mut body = body;
                loop {
                    match phase {
                        Phase::Relay {
                            mut frames,
                            winner_provider,
                            winner_model,
                            mut terminal,
                        } => match frames.next().await {
                            Some(Ok(bytes)) => {
                                let terminal_cut = terminal.scan(&bytes);
                                if let Ok(mut slot) = winner_slot.lock() {
                                    *slot = winner_provider.clone();
                                }
                                if let Ok(mut slot) = winner_model_slot.lock() {
                                    *slot = winner_model.clone();
                                }
                                match terminal_cut {
                                    // The terminal frame completes inside
                                    // this chunk: relay through its
                                    // boundary, then end the outward
                                    // stream. The still-open upstream
                                    // drains detached under the Responses
                                    // relay's budget, so the client's
                                    // stream completes at the turn — a
                                    // client waiting for EOF is never
                                    // stranded — and the keepalive wrapper
                                    // can never inject a ping past the
                                    // terminal frame while the drain runs.
                                    Some(cut) => {
                                        spawn_terminal_drain(frames);
                                        return Some((
                                            Ok::<Bytes, Infallible>(bytes.slice(..cut)),
                                            (Phase::Done, body),
                                        ));
                                    }
                                    None => {
                                        return Some((
                                            Ok::<Bytes, Infallible>(bytes),
                                            (
                                                Phase::Relay {
                                                    frames,
                                                    winner_provider,
                                                    winner_model,
                                                    terminal,
                                                },
                                                body,
                                            ),
                                        ));
                                    }
                                }
                            }
                            Some(Err(envelope)) => {
                                // A pre-terminal mid-relay body failure: emit
                                // the terminal error event and record the
                                // failure. The Sentry event is the stream
                                // observer's: it parses the `error` frame this
                                // arm emits, so capturing here would report
                                // the failure twice. A post-terminal failure
                                // never reaches this arm — the relay ended at
                                // the terminal frame and the drain absorbs
                                // it, mirroring the Responses relay's
                                // post-terminal suppression
                                // (`adapters::responses::sse_parse`).
                                let frame = sse("error", &envelope);
                                observability::record_span_outcome_on(
                                    &request_span,
                                    &winner_provider,
                                    StatusCode::BAD_GATEWAY,
                                );
                                return Some((Ok(Bytes::from(frame)), (Phase::Done, body)));
                            }
                            None => {
                                // Recorded at winner selection: the relay
                                // ended cleanly, nothing to overwrite.
                                return Some((Ok(Bytes::new()), (Phase::Done, body)));
                            }
                        },
                        Phase::Done => return None,
                        Phase::Attempt {
                            mut attempts,
                            mut remembered,
                        } => {
                            let Some((index, route)) = attempts.pop_front() else {
                                // Chain exhausted: relay the remembered best
                                // failure when one was recorded, else the
                                // synthesized 502 — mirroring the pre-commit
                                // loop's preference and exhaustion arms.
                                let (envelope, status, finish_provider, finish_model) =
                                    match remembered {
                                        Some(Remembered {
                                            envelope,
                                            status,
                                            provider,
                                            model,
                                        }) => (envelope, status, provider, model),
                                        None => (
                                            LazyEnvelope::Ready(
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
                                                .await,
                                            ),
                                            StatusCode::BAD_GATEWAY,
                                            last_provider.clone(),
                                            last_model.clone(),
                                        ),
                                    };
                                // The terminal error frame is the stream's
                                // first chunk: attribute it to the provider
                                // that supplied the failure, not the failed
                                // primary.
                                if let Ok(mut slot) = winner_slot.lock() {
                                    *slot = finish_provider.clone();
                                }
                                if let Ok(mut slot) = winner_model_slot.lock() {
                                    *slot = finish_model.clone();
                                }
                                let envelope = envelope.resolve().await;
                                crate::metrics::record_failover(&last_provider, "exhausted");
                                let frame = sse("error", &envelope);
                                // The Sentry event is the stream observer's
                                // (it parses the `error` frame this arm
                                // emits); only the span fields update here.
                                observability::record_span_outcome_on(
                                    &request_span,
                                    &finish_provider,
                                    status,
                                );
                                return Some((Ok(Bytes::from(frame)), (Phase::Done, body)));
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
                            let attempt_body = if attempts.is_empty() {
                                // The final attempt: no later route needs the
                                // buffered body, so move it instead of
                                // deep-copying its raw bytes.
                                body.take().expect("attempt phase holds the request body")
                            } else {
                                body.as_ref()
                                    .expect("attempt phase holds the request body")
                                    .clone()
                            };
                            let provider = route.provider.clone();
                            let model = route.model.clone();
                            let outcome = match route.adapter {
                                AdapterKind::Responses => {
                                    crate::adapters::responses::chain_attempt(
                                        &state,
                                        &route,
                                        &attempt_headers,
                                        attempt_body,
                                        &estimate_cache,
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
                                Attempt::Winner { headers_at, relay } => {
                                    let (start, frames) = resolve_winner(
                                        &provider,
                                        &model,
                                        headers_at
                                            .saturating_duration_since(attempt_started)
                                            .as_secs_f64()
                                            * 1000.0,
                                        &request_span,
                                        &winner_slot,
                                        &winner_model_slot,
                                        relay,
                                    )
                                    .await;
                                    let relay = Phase::Relay {
                                        frames,
                                        winner_provider: provider,
                                        winner_model: model,
                                        terminal: TerminalScan::new(),
                                    };
                                    match start {
                                        Some(start) => {
                                            return Some((Ok(start), (relay, None)));
                                        }
                                        None => {
                                            // The winner is selected: the
                                            // buffered request body must not
                                            // stay resident for the rest of
                                            // the (possibly long) relay.
                                            body = None;
                                            phase = relay;
                                            continue;
                                        }
                                    }
                                }
                                Attempt::Failed {
                                    advance,
                                    remember,
                                    envelope,
                                    status,
                                } => {
                                    crate::metrics::record_proxied_request(
                                        &provider,
                                        &model,
                                        status.as_u16(),
                                        attempt_started.elapsed().as_secs_f64() * 1000.0,
                                    );
                                    if advance {
                                        if remember {
                                            // Mirror the loop's
                                            // remembered-failure preference:
                                            // 429 > 401/403 > 404 > other 5xx.
                                            let priority =
                                                super::failover::failure_priority(status);
                                            let better =
                                                remembered.as_ref().is_none_or(|current| {
                                                    super::failover::failure_priority(
                                                        current.status,
                                                    ) < priority
                                                });
                                            if better {
                                                remembered = Some(Remembered {
                                                    envelope,
                                                    status,
                                                    provider: provider.clone(),
                                                    model: model.clone(),
                                                });
                                            }
                                        }
                                        if !attempts.is_empty() {
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
                                            phase = Phase::Attempt {
                                                attempts,
                                                remembered,
                                            };
                                            continue;
                                        }
                                        // Exhausted on an advance-worthy
                                        // failure: fall through to the
                                        // remembered/synthesis branch.
                                        phase = Phase::Attempt {
                                            attempts,
                                            remembered,
                                        };
                                        continue;
                                    }
                                    // Attribute before the terminal frame
                                    // goes out: this failure names itself,
                                    // not the failed primary.
                                    if let Ok(mut slot) = winner_slot.lock() {
                                        *slot = provider.clone();
                                    }
                                    if let Ok(mut slot) = winner_model_slot.lock() {
                                        *slot = model.clone();
                                    }
                                    let envelope = envelope.resolve().await;
                                    let frame = sse("error", &envelope);
                                    // The Sentry event is the stream
                                    // observer's (it parses the `error`
                                    // frame this arm emits); only the span
                                    // fields update here.
                                    observability::record_span_outcome_on(
                                        &request_span,
                                        &provider,
                                        status,
                                    );
                                    return Some((Ok(Bytes::from(frame)), (Phase::Done, body)));
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
    let response =
        crate::proxy::safeguards::synthesize(response, &requested_safeguards, max_request_bytes)
            .await;
    let mut response = stream_metrics::observe_response_with_slot(
        response,
        Protocol::Anthropic,
        winner_slot,
        winner_model_slot,
        started_at,
    );
    // `x-gateway-model` names the client-requested id and is correct
    // regardless of the winner; the upstream-naming headers are omitted on
    // this path — the winner is unknown at commit time, and stamping the
    // first route would contradict the header contract.
    super::failover::stamp_gateway_model_header(&mut response, &requested_model);
    // The router pair is not winner-dependent: it reports what the router chose
    // before any attempt was made, so omitting it here would have made the
    // documented "stamped for every router type" false for exactly the
    // streaming failover chains (#621 review).
    if let Some(stamp) = router_stamp.as_ref() {
        super::failover::stamp_router_headers(&mut response, stamp);
    }
    Ok((StatusCode::OK, response))
}

#[cfg(test)]
mod tests {
    use super::{is_terminal_frame, sse_frame_boundary, TerminalScan};

    fn scan(chunks: &[&str]) -> bool {
        let mut scan = TerminalScan::new();
        let mut armed = false;
        for chunk in chunks {
            if scan.scan(chunk.as_bytes()).is_some() {
                armed = true;
            }
        }
        armed
    }

    #[test]
    fn a_complete_message_stop_frame_arms_the_scan() {
        assert!(scan(&[
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        ]));
    }

    #[test]
    fn the_terminal_frame_may_straddle_chunk_boundaries() {
        assert!(scan(&[
            "event: message_st",
            "op\ndata: {\"type\":\"message_stop\"}",
            "\n\n",
        ]));
    }

    #[test]
    fn the_terminal_cut_covers_the_straddled_frame_tail() {
        let mut scan = TerminalScan::new();
        let head = "event: message_st";
        assert_eq!(scan.scan(head.as_bytes()), None);
        let chunk = "op\ndata: {\"type\":\"message_stop\"}\n\ntrailing";
        let cut = scan
            .scan(chunk.as_bytes())
            .expect("the frame completes in this chunk");
        assert_eq!(
            format!("{head}{}", &chunk[..cut]),
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            "the relay cuts exactly at the terminal frame boundary"
        );
    }

    #[test]
    fn the_terminal_cut_includes_earlier_frames_of_the_same_chunk() {
        let mut scan = TerminalScan::new();
        let chunk = "event: ping\ndata: {}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\ntrailing";
        let cut = scan
            .scan(chunk.as_bytes())
            .expect("the frame completes in this chunk");
        assert_eq!(
            &chunk[..cut],
            "event: ping\ndata: {}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
    }

    #[test]
    fn ordinary_chunks_return_no_cut() {
        let mut scan = TerminalScan::new();
        assert_eq!(scan.scan(b"event: content_block_delta\ndata: {}\n\n"), None);
        assert_eq!(scan.scan(b"event: message_st"), None);
    }

    #[test]
    fn a_crlf_terminal_frame_arms_the_scan() {
        assert!(scan(&[
            "event: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n"
        ]));
    }

    #[test]
    fn a_no_space_terminal_event_field_arms_the_scan() {
        assert!(scan(&[
            "event:message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        ]));
    }

    #[test]
    fn a_complete_error_frame_arms_the_scan() {
        assert!(scan(&[
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded\"}}\n\n"
        ]));
    }

    #[test]
    fn a_no_space_error_event_field_arms_the_scan() {
        assert!(scan(&[
            "event:error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded\"}}\n\n"
        ]));
    }

    #[test]
    fn an_error_shaped_data_payload_does_not_arm_the_scan() {
        assert!(!scan(&[
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"event: error\"}}\n\n",
        ]));
    }

    #[test]
    fn the_terminal_cut_ends_at_the_error_frame_boundary() {
        let mut scan = TerminalScan::new();
        let chunk = concat!(
            "event: content_block_delta\ndata: {}\n\n",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\"}}\n\n",
            "trailing",
        );
        let cut = scan
            .scan(chunk.as_bytes())
            .expect("the error frame completes in this chunk");
        assert_eq!(
            &chunk[..cut],
            concat!(
                "event: content_block_delta\ndata: {}\n\n",
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\"}}\n\n",
            ),
            "the relay cuts exactly at the terminal error frame boundary"
        );
    }

    #[test]
    fn ordinary_frames_do_not_arm_the_scan() {
        assert!(!scan(&[
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n",
        ]));
    }

    #[test]
    fn a_delta_payload_mentioning_message_stop_is_not_a_terminal_frame() {
        // The literal appears inside a `data:` payload — only a complete
        // frame's own `event: message_stop` line arms the scan.
        assert!(!scan(&[
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"event: message_stop\"}}\n\n",
        ]));
    }

    #[test]
    fn once_armed_the_scan_stays_armed() {
        let mut scan = TerminalScan::new();
        assert!(scan
            .scan(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")
            .is_some());
        // A second terminal frame produces no second cut: the relay ended at
        // the first.
        assert_eq!(
            scan.scan(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
            None
        );
    }

    #[test]
    fn a_stop_shaped_suffix_of_an_oversized_frame_does_not_arm() {
        // Bytes after a boundaryless oversized frame are still that frame's
        // tail until its own boundary arrives: a stop-shaped payload there
        // is content, not a terminal frame.
        let mut scan = TerminalScan::new();
        let garbage = vec![b'x'; super::MAX_TERMINAL_FRAME_BYTES + 1];
        scan.scan(&garbage);
        assert_eq!(
            scan.scan(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
            None,
            "the oversized frame is still open until a boundary closes it"
        );
    }

    #[test]
    fn scanning_resumes_after_an_oversized_frame_ends() {
        let mut scan = TerminalScan::new();
        let oversized = vec![b'x'; super::MAX_TERMINAL_FRAME_BYTES + 1];
        assert_eq!(scan.scan(&oversized), None, "the frame exceeds the bound");
        assert_eq!(
            scan.scan(b"\n\n"),
            None,
            "the skip ends at the frame boundary"
        );
        assert!(scan
            .scan(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n")
            .is_some());
    }

    #[test]
    fn the_terminal_cut_skips_a_boundary_split_across_chunks() {
        let mut scan = TerminalScan::new();
        let oversized = vec![b'x'; super::MAX_TERMINAL_FRAME_BYTES + 1];
        assert_eq!(scan.scan(&oversized), None);
        assert_eq!(
            scan.scan(b"x\n"),
            None,
            "the frame end still awaits its second newline"
        );
        let chunk = "\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\ntrailing";
        let cut = scan
            .scan(chunk.as_bytes())
            .expect("the terminal frame completes in this chunk");
        assert_eq!(
            &chunk[..cut],
            "\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            "the cut skips the split boundary and stops at the terminal frame"
        );
    }

    #[test]
    fn a_crlf_boundary_ends_the_oversized_skip() {
        let mut scan = TerminalScan::new();
        let oversized = vec![b'x'; super::MAX_TERMINAL_FRAME_BYTES + 1];
        assert_eq!(scan.scan(&oversized), None);
        assert_eq!(
            scan.scan(b"\r\n\r\n"),
            None,
            "the skip ends at the CRLF boundary"
        );
        assert!(scan
            .scan(b"event: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n")
            .is_some());
    }

    #[test]
    fn a_crlf_boundary_split_across_chunks_ends_the_oversized_skip() {
        let mut scan = TerminalScan::new();
        let mut oversized = vec![b'x'; super::MAX_TERMINAL_FRAME_BYTES];
        oversized.extend_from_slice(b"\r\n");
        assert_eq!(scan.scan(&oversized), None);
        let chunk = "\r\nevent: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\ntrailing";
        let cut = scan
            .scan(chunk.as_bytes())
            .expect("the terminal frame completes in this chunk");
        assert_eq!(
            &chunk[..cut],
            "\r\nevent: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n",
            "the cut skips the split CRLF boundary and stops at the terminal frame"
        );
    }

    #[test]
    fn the_skip_seed_carries_the_boundary_byte_from_the_oversized_chunk() {
        // The oversized chunk itself ends in the boundary's first byte: only
        // the carried tail seeded into the skip window finds the split.
        let mut scan = TerminalScan::new();
        let mut oversized = vec![b'x'; super::MAX_TERMINAL_FRAME_BYTES];
        oversized.push(b'\n');
        assert_eq!(scan.scan(&oversized), None);
        let chunk = "\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\ntrailing";
        let cut = scan
            .scan(chunk.as_bytes())
            .expect("the terminal frame completes in this chunk");
        assert_eq!(
            &chunk[..cut],
            "\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            "the cut skips the carried boundary byte and stops at the terminal frame"
        );
    }

    #[test]
    fn frame_boundaries_include_both_line_endings() {
        let mut buf = b"data: {}\n\nevent: x\r\n\r\n".to_vec();
        assert_eq!(sse_frame_boundary(&buf), Some(b"data: {}\n\n".len()));
        let rest = buf.split_off(b"data: {}\n\n".len());
        assert_eq!(sse_frame_boundary(&rest), Some(rest.len()));
        assert_eq!(sse_frame_boundary(b"no boundary yet"), None);
    }

    #[test]
    fn the_last_event_field_wins_like_the_shared_parser() {
        // The shared parser overwrites the event name per `event:` line — the
        // SSE rule — so a later line supersedes an earlier one, and the scan
        // arms exactly when the observer classifies the terminal.
        assert!(is_terminal_frame(
            b"event: message_start\nevent: message_stop\ndata: {}\n\n"
        ));
        assert!(!is_terminal_frame(
            b"event: message_stop\nevent: message_start\ndata: {}\n\n"
        ));
        assert!(is_terminal_frame(
            b"event: content_block_delta\nevent: error\ndata: {}\n\n"
        ));
        assert!(!is_terminal_frame(
            b"event: error\nevent: content_block_delta\ndata: {}\n\n"
        ));
    }

    #[test]
    fn only_the_event_line_matches_not_a_data_payload() {
        assert!(is_terminal_frame(b"event: message_stop\ndata: {}\n\n"));
        assert!(is_terminal_frame(b"event: message_stop\r\n"));
        assert!(is_terminal_frame(b"event:message_stop\n\n"));
        assert!(is_terminal_frame(b"event:message_stop\r\n"));
        assert!(is_terminal_frame(b"event: error\ndata: {}\n\n"));
        assert!(is_terminal_frame(b"event:error\r\n"));
        assert!(!is_terminal_frame(b"data: event: message_stop\n\n"));
        assert!(!is_terminal_frame(b"data: event:message_stop\n\n"));
        assert!(!is_terminal_frame(b"data: event: error\n\n"));
        // The shared field parser strips one optional post-colon space — the
        // SSE rule — so a second space stays part of the event name.
        assert!(!is_terminal_frame(b"event:  message_stop\n\n"));
        assert!(!is_terminal_frame(b"event:  error\n\n"));
        assert!(!is_terminal_frame(
            b"event: content_block_delta\ndata: {\"text\":\"event: message_stop\"}\n\n"
        ));
        assert!(!is_terminal_frame(
            b"event: content_block_delta\ndata: {\"text\":\"event: error\"}\n\n"
        ));
    }

    #[tokio::test]
    async fn the_winner_is_recorded_before_a_pending_relay_build_resolves() {
        use crate::metrics::proxied_request_samples_for_tests;
        use std::collections::HashMap;
        use std::future::Future;
        use tracing_subscriber::layer::{Layer, SubscriberExt};
        use tracing_subscriber::Registry;

        #[derive(Clone, Default)]
        struct CapturingLayer(std::sync::Arc<std::sync::Mutex<HashMap<String, String>>>);
        struct CapturingVisitor<'a>(&'a mut HashMap<String, String>);
        impl tracing::field::Visit for CapturingVisitor<'_> {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.insert(field.name().to_string(), value.to_string());
            }
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                self.0.insert(field.name().to_string(), value.to_string());
            }
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0
                    .insert(field.name().to_string(), format!("{value:?}"));
            }
        }
        impl<S: tracing::Subscriber> Layer<S> for CapturingLayer {
            fn on_record(
                &self,
                _id: &tracing::span::Id,
                values: &tracing::span::Record<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut map = self.0.lock().expect("capturing layer mutex poisoned");
                values.record(&mut CapturingVisitor(&mut map));
            }
        }

        let provider = "resolve-winner-test-provider".to_string();
        let model = "resolve-winner-test-model".to_string();
        let provider_slot =
            std::sync::Arc::new(std::sync::Mutex::new("failed-primary".to_string()));
        let model_slot = std::sync::Arc::new(std::sync::Mutex::new("failed-model".to_string()));
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        let build_resolved = std::sync::Arc::new(std::sync::Mutex::new(false));
        let build_resolved_flag = build_resolved.clone();
        let relay = super::RelayBuild::Pending(Box::pin(async move {
            let _ = wait.await;
            *build_resolved_flag.lock().expect("build flag lock") = true;
            let frames: super::ClientFrames = Box::pin(futures_util::stream::empty());
            (axum::body::Bytes::new(), frames)
        }));

        let captured = CapturingLayer::default();
        let subscriber = Registry::default().with(captured.clone());
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "test_winner",
                shunt.provider = tracing::field::Empty,
                http.response.status_code = tracing::field::Empty,
                otel.status_code = tracing::field::Empty,
            );
            let mut resolution = Box::pin(super::resolve_winner(
                &provider,
                &model,
                0.0,
                &span,
                &provider_slot,
                &model_slot,
                relay,
            ));
            // One poll runs every record up to the pending build's await:
            // the winner must be fully attributed — slots, requests sample,
            // span fields — while the build is still blocked. The capture
            // call on the winner's 200 is a documented no-op (its own tests
            // pin the no-op), so its position is unobservable on this path.
            let mut polled = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(
                resolution.as_mut().poll(&mut polled).is_pending(),
                "the pending build keeps the resolution pending"
            );
            let map = captured.0.lock().expect("capturing layer mutex poisoned");
            assert_eq!(
                map.get("shunt.provider").map(String::as_str),
                Some(provider.as_str()),
                "the span carries the winner before the pending build resolves"
            );
            assert_eq!(
                map.get("http.response.status_code").map(String::as_str),
                Some("200")
            );
            assert_eq!(map.get("otel.status_code").map(String::as_str), Some("ok"));
            drop(map);
            assert_eq!(*provider_slot.lock().expect("provider slot lock"), provider);
            assert_eq!(*model_slot.lock().expect("model slot lock"), model);
            let (count, _) = proxied_request_samples_for_tests(&provider, &model, 200);
            assert_eq!(
                count, 1,
                "the requests sample is recorded before the pending build resolves"
            );
            assert!(
                !*build_resolved.lock().expect("build flag lock"),
                "the pending build stays unawaited while the winner records"
            );
            release.send(()).expect("the build is still listening");
            loop {
                if let std::task::Poll::Ready(_resolved) = resolution.as_mut().poll(&mut polled) {
                    break;
                }
            }
            assert!(
                *build_resolved.lock().expect("build flag lock"),
                "the pending build resolves once released"
            );
        });
    }
}
