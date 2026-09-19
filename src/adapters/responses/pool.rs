//! Codex/ChatGPT OAuth account-pool failover (M10): try each account in turn,
//! websocket-first when enabled with an HTTP fallback per account, classifying
//! each raw upstream status to decide relay / rotate / refresh-and-retry.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use axum::http::{HeaderValue, StatusCode};
use futures_util::{stream, Stream, StreamExt};
use serde_json::Value;

use crate::{
    accounts::{self, FailoverAction, ReprobeReservation},
    adapters::AdapterError,
    auth::{self, codex::auth::CodexAuthStore, resolve_chatgpt_account, Credential},
    config::{AccountConfig, AuthMode},
    model::responses::ResponseEvent,
    routing::Route,
    server::AppState,
};

use super::body::{prepare_body, PreparedBody};
use super::context::{CredentialSource, ForwardOptions, PoolForward, RelayOptions, TurnOptions};
use super::early_stream::{
    bounded_input_estimate, estimated_machine_factory, http_events_stream, parsed_events,
    pool_streaming_response, HttpSendContext, PoolEvent, PoolItem,
};
use super::error::{adapter_error_envelope, mapped_upstream_error, own_error, transport_error};
use super::http::{http_send, json_response, stream_response};
use super::websocket::forward_websocket;
use crate::proxy::chain_stream::LazyEnvelope;

fn select_pool_order(
    state: &AppState,
    provider: &str,
    accounts: &[AccountConfig],
    session_id: Option<&str>,
    upstream_model: &str,
    ws_enabled: bool,
) -> Vec<usize> {
    debug_assert!(
        ws_enabled,
        "non-WebSocket selection uses deferred reservation"
    );
    state.accounts.select_order_without_reprobe(
        provider,
        accounts,
        session_id,
        Some(upstream_model),
        state.config.server.pool.as_ref(),
    )
}

pub(super) fn cancel_reprobe_for_account(
    reservation: &mut Option<ReprobeReservation>,
    selected_index: usize,
) {
    if reservation
        .as_ref()
        .is_some_and(|reservation| reservation.selected_index() == selected_index)
    {
        if let Some(reservation) = reservation.as_mut() {
            reservation.cancel();
        }
    }
}

pub(super) fn commit_reprobe_for_account(
    reservation: &mut Option<ReprobeReservation>,
    selected_index: usize,
) {
    if reservation
        .as_ref()
        .is_some_and(|reservation| reservation.selected_index() == selected_index)
    {
        if let Some(reservation) = reservation.as_mut() {
            reservation.commit();
        }
    }
}

/// Everything the streaming pool producer needs to run the account loop inside
/// the already-committed stream.
pub(super) struct PoolStreamContext {
    pub(super) state: AppState,
    pub(super) route: Route,
    pub(super) auth: AuthMode,
    pub(super) session_id: Option<String>,
    pub(super) upstream_body: std::sync::Arc<Value>,
    pub(super) accounts_config: std::sync::Arc<Vec<AccountConfig>>,
    pub(super) order: Vec<usize>,
    pub(super) reprobe: Option<ReprobeReservation>,
    pub(super) ramp_initial: Option<u32>,
    /// Whether this pool stream owns the request's `record_proxied_request`
    /// sample (the committed single-route paths) or the enclosing chain does.
    pub(super) record_metrics: bool,
    /// A pre-commit instant the sample clock starts at when the stream owns
    /// it — the committed pool's account scan ran inside this stream — else
    /// the first poll of the account loop.
    pub(super) started_at: Option<std::time::Instant>,
}

/// One streaming pool turn's event feed: the account loop (admission,
/// credential resolution, send, classification, refresh-and-retry) runs inside
/// the stream after the early commit, and the winning account's parsed events
/// are yielded one at a time. Every terminal failure — the TTFB timeout, a
/// non-failover non-2xx status, or pool exhaustion — becomes the same Anthropic
/// error envelope the pre-commit path returned as a JSON body, emitted as one
/// terminal SSE `error` event by [`pool_streaming_response`].
///
/// The request-derived fields the committed streaming `chatgpt_oauth` turn
/// needs beyond `state`/`route`: the deferred account scan, the
/// single-credential fallback's deferred credential, and the shared
/// translation inputs.
pub(super) struct PoolForwardStream {
    pub session_id: Option<String>,
    pub upstream_body: std::sync::Arc<Value>,
    pub turn: TurnOptions,
    pub estimate_input: Option<std::sync::Arc<Value>>,
    pub scan: std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<AccountConfig>, String>> + Send>,
    >,
    pub credential: CredentialSource,
}

/// The committed variant of [`forward_chatgpt_oauth`] for streaming turns
/// without the websocket transport: the response commits before the account
/// scan, so a slow filesystem scan or a saturated blocking pool cannot starve
/// the client of headers and keepalive pings. The scan, the all-disabled
/// check, the empty-scan single-credential fallback, and the pool itself all
/// run inside the stream ([`pool_or_single_events`]).
pub(super) async fn forward_chatgpt_oauth_stream(
    state: AppState,
    route: Route,
    forward: PoolForwardStream,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let PoolForwardStream {
        session_id,
        upstream_body,
        turn,
        estimate_input,
        scan,
        credential,
    } = forward;
    let keepalive = Duration::from_secs(state.config.server.sse_keepalive_seconds);
    let route_for_start = route.clone();
    let single = HttpSendContext {
        state: state.clone(),
        route: route.clone(),
        policy: state
            .config
            .provider(&route.provider)
            .map(|provider| provider.retry.policy())
            .unwrap_or(crate::retry::RetryPolicy::DISABLED),
        credential: None,
        session_id: session_id.clone(),
        upstream_body: upstream_body.clone(),
        auth: AuthMode::ChatgptOauth,
        codex_quota_account: None,
    };
    let events = pool_or_single_events(
        scan,
        state.clone(),
        route.clone(),
        session_id,
        upstream_body,
        single,
        credential,
    );
    Ok((
        StatusCode::OK,
        pool_streaming_response(
            estimated_machine_factory(turn, route_for_start, estimate_input),
            keepalive,
            events,
        ),
    ))
}

/// One committed stream for a streaming `chatgpt_oauth` turn: the account
/// scan, the all-disabled check, the empty-scan single-credential fallback,
/// and the pool itself all run inside the committed response, so neither a
/// slow filesystem scan nor a saturated blocking pool can delay the commit
/// (keepalive pings cover the wait).
pub(super) fn pool_or_single_events(
    scan: std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<AccountConfig>, String>> + Send>,
    >,
    state: AppState,
    route: Route,
    session_id: Option<String>,
    upstream_body: std::sync::Arc<Value>,
    single: HttpSendContext,
    single_credential: CredentialSource,
) -> impl Stream<Item = Result<PoolItem, Value>> + Send + 'static {
    type Inner = std::pin::Pin<Box<dyn Stream<Item = Result<PoolItem, Value>> + Send>>;
    struct Init {
        scan: std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Vec<AccountConfig>, String>> + Send>,
        >,
        state: AppState,
        route: Route,
        session_id: Option<String>,
        upstream_body: std::sync::Arc<Value>,
        single: HttpSendContext,
        single_credential: CredentialSource,
    }
    enum Phase {
        Init(Box<Init>),
        Inner(Inner),
        Done,
    }
    stream::unfold(
        Phase::Init(Box::new(Init {
            scan,
            state,
            route,
            session_id,
            upstream_body,
            single,
            single_credential,
        })),
        move |phase| async move {
            match phase {
                Phase::Init(init) => {
                    let attempt_started = Instant::now();
                    let Init {
                        scan,
                        state,
                        route,
                        session_id,
                        upstream_body,
                        single,
                        single_credential,
                    } = *init;
                    let accounts = match scan.await {
                        Ok(accounts) => accounts,
                        Err(error) => {
                            // A scan failure is a classified gateway
                            // failure (502), recorded here so the committed
                            // response does not leave the request unsampled —
                            // the chain records the same shape for its own
                            // scan failure.
                            crate::metrics::record_proxied_request(
                                &route.provider,
                                &route.model,
                                StatusCode::BAD_GATEWAY.as_u16(),
                                attempt_started.elapsed().as_secs_f64() * 1000.0,
                            );
                            let envelope = adapter_error_envelope(own_error(error)).await;
                            return Some((Err(envelope), Phase::Done));
                        }
                    };
                    if !accounts.is_empty() && accounts.iter().all(|account| account.disabled) {
                        crate::metrics::record_proxied_request(
                            &route.provider,
                            &route.model,
                            StatusCode::BAD_GATEWAY.as_u16(),
                            attempt_started.elapsed().as_secs_f64() * 1000.0,
                        );
                        let envelope = adapter_error_envelope(own_error(format!(
                            "provider '{}' has {} account(s) but all are `disabled = true`; none are selectable",
                            route.provider,
                            accounts.len()
                        )))
                        .await;
                        return Some((Err(envelope), Phase::Done));
                    }
                    let ramp_initial = state.config.storm_ramp_initial();
                    let mut inner: Inner = if accounts.is_empty() {
                        Box::pin(
                            http_events_stream(
                                single,
                                single_credential,
                                None,
                                Some(attempt_started),
                            )
                            .map(|item| item.map(|event| PoolItem::Event(PoolEvent::Event(event)))),
                        )
                    } else {
                        let accounts_config = std::sync::Arc::new(accounts);
                        let (order, reprobe) = state.accounts.select_order_deferred(
                            &route.provider,
                            &accounts_config,
                            session_id.as_deref(),
                            Some(route.upstream_model.as_str()),
                            state.config.server.pool.as_ref(),
                        );
                        Box::pin(pool_events_stream(PoolStreamContext {
                            state,
                            route,
                            auth: AuthMode::ChatgptOauth,
                            session_id,
                            upstream_body,
                            accounts_config,
                            order,
                            reprobe,
                            ramp_initial,
                            record_metrics: true,
                            started_at: Some(attempt_started),
                        }))
                    };
                    inner.next().await.map(|item| (item, Phase::Inner(inner)))
                }
                Phase::Inner(mut inner) => {
                    inner.next().await.map(|item| (item, Phase::Inner(inner)))
                }
                Phase::Done => None,
            }
        },
    )
}

/// The winning account's admission guard rides in the relay phase so the
/// storm-control slot
/// stays held until the stream ends. Mirrors the ws-fallback/non-streaming
/// loop below arm for arm; the duplication is deliberate until the websocket
/// transport joins the early commit. The committed response
/// cannot carry the winning account's `x-shunt-account` header — the headers
/// go out with the synthetic start, before the account is known — so the
/// winner is attributed with an `event: account` frame before its first
/// relayed frame instead; the non-streaming and websocket paths still attach
/// the header.
pub(super) fn pool_events_stream(
    context: PoolStreamContext,
) -> impl Stream<Item = Result<PoolItem, Value>> + Send + 'static {
    let PoolStreamContext {
        state,
        route,
        auth,
        session_id,
        upstream_body,
        accounts_config,
        order,
        reprobe,
        ramp_initial,
        record_metrics,
        started_at,
    } = context;
    type Parsed = std::pin::Pin<Box<dyn Stream<Item = Result<ResponseEvent, Value>> + Send>>;
    enum Phase {
        NextAccount,
        Relay {
            parsed: Parsed,
            guard: Option<accounts::AdmissionGuard>,
            account: Option<String>,
        },
        Done,
    }
    let candidates = order.len();
    stream::unfold(
        (
            Phase::NextAccount,
            order.into_iter().enumerate(),
            None::<PreparedBody>,
            None::<reqwest::Response>,
            reprobe,
            started_at,
        ),
        move |(phase, order_iter, http_body, last_response, reprobe, attempt_started)| {
            let state = state.clone();
            let route = route.clone();
            let accounts_config = accounts_config.clone();
            let session_id = session_id.clone();
            let upstream_body = upstream_body.clone();
            async move {
                let mut phase = phase;
                let mut order_iter = order_iter;
                let mut http_body = http_body;
                let mut last_response = last_response;
                let mut reprobe = reprobe;
                let mut attempt_started = attempt_started;
                loop {
                    match phase {
                        Phase::Relay {
                            mut parsed,
                            guard,
                            mut account,
                        } => {
                            // Attribute the winner once, before its first
                            // relayed frame.
                            if let Some(name) = account.take() {
                                return Some((
                                    Ok(PoolItem::Event(PoolEvent::Account(name))),
                                    (
                                        Phase::Relay {
                                            parsed,
                                            guard,
                                            account,
                                        },
                                        order_iter,
                                        http_body,
                                        last_response,
                                        reprobe,
                                        attempt_started,
                                    ),
                                ));
                            }
                            match parsed.next().await {
                                Some(Ok(event)) => {
                                    return Some((
                                        Ok(PoolItem::Event(PoolEvent::Event(event))),
                                        (
                                            Phase::Relay {
                                                parsed,
                                                guard,
                                                account,
                                            },
                                            order_iter,
                                            http_body,
                                            last_response,
                                            reprobe,
                                            attempt_started,
                                        ),
                                    ));
                                }
                                Some(Err(envelope)) => {
                                    return Some((
                                        Err(envelope),
                                        (
                                            Phase::Done,
                                            order_iter,
                                            http_body,
                                            last_response,
                                            reprobe,
                                            attempt_started,
                                        ),
                                    ));
                                }
                                None => return None,
                            }
                        }
                        Phase::NextAccount => {
                            // The response committed before the account loop
                            // ran: the failover loop's dispatch-time sample
                            // would read a fake 200 and a near-zero elapsed
                            // (skipped via `InStreamMetrics`); the real
                            // sample lands here, once per committed attempt,
                            // when the pool classifies its outcome — matching
                            // the chain's per-attempt record. The chain
                            // builds its own pool streams with
                            // `record_metrics: false` and records the attempt
                            // itself. A caller that ran pre-commit work
                            // inside the committed stream (the committed
                            // pool's account scan) seeds the clock at that
                            // work's start.
                            let started = *attempt_started.get_or_insert_with(Instant::now);
                            let record = |status: StatusCode| {
                                if record_metrics {
                                    crate::metrics::record_proxied_request(
                                        &route.provider,
                                        &route.model,
                                        status.as_u16(),
                                        started.elapsed().as_secs_f64() * 1000.0,
                                    );
                                }
                            };
                            let Some((position, index)) = order_iter.next() else {
                                crate::metrics::record_pool_rotation(&route.provider, "exhausted");
                                // Classified pre-frame exhaustion: the
                                // committed chain advances on it exactly like
                                // the pre-commit loop (§4) — a relayed
                                // advance status is advance-worthy and
                                // remembered, transport exhaustion advances
                                // without remembering.
                                let (status, advance, remember, envelope) =
                                    match last_response.take() {
                                        Some(upstream) => {
                                            let status = upstream.status();
                                            let envelope =
                                                LazyEnvelope::Deferred(Box::pin(async move {
                                                    adapter_error_envelope(
                                                        mapped_upstream_error(
                                                            status, upstream, auth,
                                                        )
                                                        .await,
                                                    )
                                                    .await
                                                }));
                                            (
                                                status,
                                                crate::proxy::failover::is_advance_status(status),
                                                true,
                                                envelope,
                                            )
                                        }
                                        None => (
                                            StatusCode::BAD_GATEWAY,
                                            true,
                                            false,
                                            LazyEnvelope::Ready(
                                                adapter_error_envelope(transport_error(
                                                    "all Codex OAuth accounts failed before receiving an upstream response"
                                                        .to_string(),
                                                ))
                                                .await,
                                            ),
                                        ),
                                    };
                                record(status);
                                return Some((
                                    Ok(PoolItem::Exhausted {
                                        status,
                                        advance,
                                        remember,
                                        envelope,
                                    }),
                                    (
                                        Phase::Done,
                                        order_iter,
                                        http_body,
                                        last_response,
                                        reprobe,
                                        attempt_started,
                                    ),
                                ));
                            };
                            let account = &accounts_config[index];
                            let Some((admission, credential)) = admit_and_resolve(
                                &state,
                                &route,
                                account,
                                ramp_initial,
                                position,
                                candidates,
                            )
                            .await
                            else {
                                cancel_reprobe_for_account(&mut reprobe, index);
                                continue;
                            };

                            let body = match &http_body {
                                Some(body) => body.clone(),
                                None => {
                                    let prepared =
                                        prepare_body(&state, &route, upstream_body.as_ref()).await;
                                    http_body = Some(prepared.clone());
                                    prepared
                                }
                            };
                            // Commit at the dispatch boundary (mirrors the loop):
                            // after admission, credential, and body preparation
                            // have all succeeded.
                            commit_reprobe_for_account(&mut reprobe, index);
                            let upstream = match http_send(
                                &state,
                                &route,
                                credential.clone(),
                                session_id.as_deref(),
                                body.clone(),
                            )
                            .await
                            {
                                Ok(response) => response,
                                Err(error @ crate::upstream_timeout::SendError::Timeout) => {
                                    let envelope =
                                        adapter_error_envelope(error.into_adapter_error(|error| {
                                            transport_error(error.without_url().to_string())
                                        }))
                                        .await;
                                    // The TTFB timeout is the configured 504
                                    // answer: terminal, never advanced (the
                                    // pre-commit loop's mapping).
                                    record(StatusCode::GATEWAY_TIMEOUT);
                                    return Some((
                                        Ok(PoolItem::Exhausted {
                                            status: StatusCode::GATEWAY_TIMEOUT,
                                            advance: false,
                                            remember: false,
                                            envelope: LazyEnvelope::Ready(envelope),
                                        }),
                                        (
                                            Phase::Done,
                                            order_iter,
                                            http_body,
                                            last_response,
                                            reprobe,
                                            attempt_started,
                                        ),
                                    ));
                                }
                                Err(crate::upstream_timeout::SendError::Transport(error)) => {
                                    state.accounts.cooldown(
                                        &route.provider,
                                        account,
                                        Duration::from_secs(30),
                                        "transport",
                                    );
                                    tracing::warn!(
                                        provider = %route.provider,
                                        account = %account.name,
                                        error = %error.without_url(),
                                        "ChatGPT OAuth upstream request failed"
                                    );
                                    continue;
                                }
                            };

                            state.accounts.note_codex_quota(
                                &route.provider,
                                account,
                                upstream.headers(),
                            );
                            match classify_first(&state, &route, account, upstream) {
                                FirstOutcome::Relay(upstream) => {
                                    let status = upstream.status();
                                    state.accounts.mark_healthy(
                                        &route.provider,
                                        account,
                                        status.is_success(),
                                    );
                                    if status.is_success() {
                                        record(StatusCode::OK);
                                        let parsed: Parsed =
                                            Box::pin(parsed_events(upstream.bytes_stream()));
                                        phase = Phase::Relay {
                                            parsed,
                                            guard: admission,
                                            account: Some(account.name.clone()),
                                        };
                                    } else {
                                        // A non-failover 4xx (e.g. 400) is a
                                        // client error, not the account's fault:
                                        // relay it re-shaped, as everywhere on
                                        // this path. Terminal, carrying its
                                        // real status for metrics.
                                        record(status);
                                        let envelope = adapter_error_envelope(
                                            mapped_upstream_error(status, upstream, auth).await,
                                        )
                                        .await;
                                        return Some((
                                            Ok(PoolItem::Exhausted {
                                                status,
                                                advance: false,
                                                remember: true,
                                                envelope: LazyEnvelope::Ready(envelope),
                                            }),
                                            (
                                                Phase::Done,
                                                order_iter,
                                                http_body,
                                                last_response,
                                                reprobe,
                                                attempt_started,
                                            ),
                                        ));
                                    }
                                }
                                FirstOutcome::Rotate(upstream) => {
                                    last_response = Some(upstream);
                                }
                                FirstOutcome::NeedRefresh(upstream) => {
                                    let retry_credential = match force_refresh_or_cooldown(
                                        &state,
                                        &route,
                                        account,
                                        &credential,
                                    )
                                    .await
                                    {
                                        Some(credential) => credential,
                                        None => {
                                            last_response = Some(upstream);
                                            continue;
                                        }
                                    };
                                    let retry = match http_send(
                                        &state,
                                        &route,
                                        retry_credential,
                                        session_id.as_deref(),
                                        body.clone(),
                                    )
                                    .await
                                    {
                                        Ok(response) => response,
                                        Err(
                                            error @ crate::upstream_timeout::SendError::Timeout,
                                        ) => {
                                            let envelope = adapter_error_envelope(
                                                error.into_adapter_error(|error| {
                                                    transport_error(error.without_url().to_string())
                                                }),
                                            )
                                            .await;
                                            // Terminal, never advanced (the
                                            // pre-commit loop's mapping).
                                            record(StatusCode::GATEWAY_TIMEOUT);
                                            return Some((
                                                Ok(PoolItem::Exhausted {
                                                    status: StatusCode::GATEWAY_TIMEOUT,
                                                    advance: false,
                                                    remember: false,
                                                    envelope: LazyEnvelope::Ready(envelope),
                                                }),
                                                (
                                                    Phase::Done,
                                                    order_iter,
                                                    http_body,
                                                    last_response,
                                                    reprobe,
                                                    attempt_started,
                                                ),
                                            ));
                                        }
                                        Err(crate::upstream_timeout::SendError::Transport(
                                            error,
                                        )) => {
                                            state.accounts.cooldown(
                                                &route.provider,
                                                account,
                                                Duration::from_secs(30),
                                                "transport",
                                            );
                                            tracing::warn!(
                                                provider = %route.provider,
                                                account = %account.name,
                                                error = %error.without_url(),
                                                "ChatGPT OAuth refresh retry failed"
                                            );
                                            last_response = Some(upstream);
                                            continue;
                                        }
                                    };
                                    state.accounts.note_codex_quota(
                                        &route.provider,
                                        account,
                                        retry.headers(),
                                    );
                                    match classify_retry(&state, &route, account, retry) {
                                        RetryOutcome::Relay(retry) => {
                                            let retry_status = retry.status();
                                            if retry_status.is_success() {
                                                state.accounts.mark_healthy(
                                                    &route.provider,
                                                    account,
                                                    true,
                                                );
                                                record(StatusCode::OK);
                                                let parsed: Parsed =
                                                    Box::pin(parsed_events(retry.bytes_stream()));
                                                phase = Phase::Relay {
                                                    parsed,
                                                    guard: admission,
                                                    account: Some(account.name.clone()),
                                                };
                                            } else {
                                                let envelope = adapter_error_envelope(
                                                    mapped_upstream_error(
                                                        retry_status,
                                                        retry,
                                                        auth,
                                                    )
                                                    .await,
                                                )
                                                .await;
                                                // Terminal, carrying its real
                                                // status for metrics.
                                                record(retry_status);
                                                return Some((
                                                    Ok(PoolItem::Exhausted {
                                                        status: retry_status,
                                                        advance: false,
                                                        remember: true,
                                                        envelope: LazyEnvelope::Ready(envelope),
                                                    }),
                                                    (
                                                        Phase::Done,
                                                        order_iter,
                                                        http_body,
                                                        last_response,
                                                        reprobe,
                                                        attempt_started,
                                                    ),
                                                ));
                                            }
                                        }
                                        RetryOutcome::Rotate(retry) => {
                                            last_response = Some(retry);
                                        }
                                    }
                                }
                            }
                        }
                        Phase::Done => return None,
                    }
                }
            }
        },
    )
}

/// Drive a Responses turn over the Codex/ChatGPT OAuth account pool (M10),
/// mirroring the Anthropic adapter's `forward_claude_oauth` as closely as this
/// adapter's structure allows. Each account in `order` is tried in turn:
/// websocket first when enabled (with the pool key prefixed per-account so
/// accounts never share a pooled connection — this is the key correctness
/// requirement of the WS integration), falling back to HTTP for that same
/// account on a pre-stream websocket failure, then classifying the raw HTTP
/// status with [`accounts::classify_codex`] to decide whether to relay,
/// rotate to the next account, or force-refresh and retry the same one.
/// Codex quota headers recorded by `note_codex_quota` feed both the admin
/// dashboard and quota-aware selection (issue #195).
pub(super) async fn forward_chatgpt_oauth(
    state: AppState,
    route: Route,
    forward: PoolForward,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let PoolForward {
        pool_key,
        session_id,
        upstream_body,
        accounts_config,
        turn,
        estimate_input,
    } = forward;
    // An all-`disabled` pool yields an empty order; surface it as a distinct
    // config error rather than the generic "all accounts failed" below.
    if !accounts_config.is_empty() && accounts_config.iter().all(|account| account.disabled) {
        tracing::warn!(
            provider = %route.provider,
            accounts = accounts_config.len(),
            "all accounts for provider are disabled; none are selectable"
        );
        return Err(own_error(format!(
            "provider '{}' has {} account(s) but all are `disabled = true`; none are selectable",
            route.provider,
            accounts_config.len()
        )));
    }
    // Codex usage recorded via note_codex_quota (x-codex-* windows) feeds
    // selection: the pool proactively rotates off near-quota accounts and
    // orders by burn-rate headroom, exactly like the Claude pool (issue #195).
    // Codex has no fable-scoped window, so the model only picks the shared
    // weekly bucket.
    let ws_enabled = state.config.codex_websocket_enabled(&route.provider);
    if turn.client_wants_stream && !ws_enabled {
        // Commit the SSE response before the account loop runs: the synthetic
        // `message_start` feeds the client's stall watchdog while admission,
        // credential resolution, and the send still run inside the stream. The
        // estimate must land in that first snapshot, so encode it up front
        // rather than overlapping it with the upstream round-trip as the
        // loop-based paths do.
        let keepalive = Duration::from_secs(state.config.server.sse_keepalive_seconds);
        let route_for_start = route.clone();
        let (order, reprobe) = state.accounts.select_order_deferred(
            &route.provider,
            &accounts_config,
            session_id.as_deref(),
            Some(route.upstream_model.as_str()),
            state.config.server.pool.as_ref(),
        );
        let events = pool_events_stream(PoolStreamContext {
            state: state.clone(),
            route: route.clone(),
            auth: AuthMode::ChatgptOauth,
            session_id,
            upstream_body: upstream_body.clone(),
            accounts_config: std::sync::Arc::new(accounts_config),
            order,
            reprobe,
            ramp_initial: state.config.storm_ramp_initial(),
            record_metrics: true,
            started_at: None,
        });
        return Ok((
            StatusCode::OK,
            pool_streaming_response(
                estimated_machine_factory(turn, route_for_start, estimate_input),
                keepalive,
                events,
            ),
        ));
    }
    let (order, mut reprobe_reservation) = if ws_enabled {
        (
            select_pool_order(
                &state,
                &route.provider,
                &accounts_config,
                session_id.as_deref(),
                &route.upstream_model,
                true,
            ),
            None,
        )
    } else {
        state.accounts.select_order_deferred(
            &route.provider,
            &accounts_config,
            session_id.as_deref(),
            Some(route.upstream_model.as_str()),
            state.config.server.pool.as_ref(),
        )
    };
    let auth = AuthMode::ChatgptOauth;
    let ramp_initial = state.config.storm_ramp_initial();
    let candidates = order.len();
    let mut last_response: Option<reqwest::Response> = None;
    // The translated request is immutable across account attempts, so serialize
    // (and, on the ChatGPT backend, zstd-compress — issue #285) it at most once
    // per turn and give each attempt (including a 401 refresh retry) a cheap
    // refcount clone. Keep this lazy so a successful websocket turn never pays to
    // prepare an HTTP body it does not send (issue #251).
    let mut http_body: Option<PreparedBody> = None;
    // Mirrors `http_body` above: the tiktoken estimate of the (unchanging)
    // translated request is spawned onto the blocking pool at most once per
    // turn and reused across every account attempt and the refresh retry,
    // rather than re-encoded per rotation. Spawned only for HTTP dispatch —
    // each websocket attempt gets its own `estimate_input` clone and does its
    // own internal spawn_blocking in `forward_websocket`, exactly like the
    // single-account path; a websocket attempt that then falls back to HTTP
    // pays a second, rare, off-executor encode, which mirrors the accepted
    // single-account fallback cost documented in `forward_websocket`.
    let mut estimate_handle: Option<tokio::task::JoinHandle<u64>> = None;

    for (position, index) in order.into_iter().enumerate() {
        let account = &accounts_config[index];

        // Storm-control admission + credential resolution (issue #195): a
        // saturated identity or failed auth rotates to the next candidate
        // (see `admit_and_resolve`); on a relayed success the guard moves
        // into the response body (`with_admission`), so the slot stays held
        // until the stream actually finishes.
        let Some((admission, credential)) =
            admit_and_resolve(&state, &route, account, ramp_initial, position, candidates).await
        else {
            cancel_reprobe_for_account(&mut reprobe_reservation, index);
            continue;
        };

        // Prefixing the pool key with the account name is the key point of
        // this integration: without it, two accounts serving the same client
        // session could reuse (and leak turn state across) one another's
        // pooled websocket connection.
        let account_pool_key = pool_key
            .as_deref()
            .map(|key| format!("{}::{key}", account.name));

        if ws_enabled {
            match forward_websocket(
                &state,
                &route,
                account_pool_key.as_deref(),
                ForwardOptions {
                    upstream_body: upstream_body.clone(),
                    auth,
                    turn,
                    codex_quota_account: Some(account.clone()),
                    // Each account attempt gets its own cheap Arc clone;
                    // forward_websocket spawns its own blocking encode from it,
                    // identical to the single-account path (see ForwardOptions).
                    estimate_input: estimate_input.clone(),
                    started_at: None,
                },
                credential.clone(),
            )
            .await
            {
                Ok((status, response)) => {
                    state
                        .accounts
                        .mark_healthy(&route.provider, account, status.is_success());
                    let response = crate::adapters::with_admission(response, admission);
                    return Ok((status, with_account_header(response, &account.name)));
                }
                Err(error) if error.failure.is_some() => {
                    // A pre-stream websocket failure (connect/handshake/send) falls
                    // back to HTTP on the SAME account, exactly like the
                    // single-account path in `forward` — only an HTTP failure
                    // triggers account-pool failover below.
                    tracing::warn!(
                        provider = %route.provider,
                        account = %account.name,
                        error = %error.message,
                        "codex websocket failed before streaming; falling back to HTTP for this account"
                    );
                }
                Err(error) => return Err(error),
            }
        }

        // Prepared once per turn and reused by every later attempt: cloning it
        // is a refcount bump, whereas re-preparing would re-serialize and
        // re-compress the same body on each rotation. Borrow rather than clone
        // here — `get_or_insert_with` cannot be used directly since preparing
        // the body is `async`, but populating `http_body` first and then
        // borrowing it keeps the happy path (already-prepared) to the single
        // refcount bump each `http_send` call below already pays, instead of
        // one bump here plus another at each call site.
        if http_body.is_none() {
            http_body = Some(prepare_body(&state, &route, upstream_body.as_ref()).await);
        }
        let body = http_body.as_ref().expect("just populated above");
        // Spawned once, right before the first HTTP send, so the CPU-bound
        // tiktoken encode overlaps this attempt's connect/RTT instead of
        // delaying it (same overlap discipline as forward_http/forward_websocket).
        if estimate_handle.is_none() {
            if let Some(request) = estimate_input.clone() {
                estimate_handle = Some(tokio::task::spawn_blocking(move || {
                    crate::count_tokens::count_input_tokens_value(&request)
                }));
            }
        }
        // Commit at the dispatch boundary, after all admission, credential,
        // body-preparation, and estimate setup has succeeded. A reservation
        // that never reaches this call is cancelled instead of consuming the
        // reprobe interval.
        commit_reprobe_for_account(&mut reprobe_reservation, index);
        let upstream = match http_send(
            &state,
            &route,
            credential.clone(),
            session_id.as_deref(),
            body.clone(),
        )
        .await
        {
            Ok(response) => response,
            Err(error @ crate::upstream_timeout::SendError::Timeout) => {
                return Err(error.into_adapter_error(|error| transport_error(error.to_string())));
            }
            Err(crate::upstream_timeout::SendError::Transport(error)) => {
                state.accounts.cooldown(
                    &route.provider,
                    account,
                    Duration::from_secs(30),
                    "transport",
                );
                tracing::warn!(
                    provider = %route.provider,
                    account = %account.name,
                    error = %error.without_url(),
                    "ChatGPT OAuth upstream request failed"
                );
                continue;
            }
        };

        state
            .accounts
            .note_codex_quota(&route.provider, account, upstream.headers());
        match classify_first(&state, &route, account, upstream) {
            FirstOutcome::Relay(upstream) => {
                // A non-401/429/5xx response means the account itself is fine,
                // whether or not this particular request succeeded (mirrors the
                // Anthropic adapter's top-level Relay arm) — but only a real
                // success grows the storm-control allowance.
                let status = upstream.status();
                state
                    .accounts
                    .mark_healthy(&route.provider, account, status.is_success());
                if status.is_success() {
                    let input_tokens_estimate = take_estimate(&mut estimate_handle).await;
                    let response = relay_success(
                        &state,
                        upstream,
                        turn.client_wants_stream,
                        turn.relay(&route),
                        input_tokens_estimate,
                    )
                    .await?;
                    let response = crate::adapters::with_admission(
                        with_account_header(response, &account.name),
                        admission,
                    );
                    // Surface the real status (a `502` when a backend error event
                    // fired on the non-streaming path, issue #113) to the access
                    // log and metrics rather than a hardcoded `200`.
                    return Ok((response.status(), response));
                }
                // A non-failover 4xx (e.g. 400) is a client error, not the
                // account's fault: relay it (re-shaped into the Anthropic error
                // envelope by mapped_upstream_error, as everywhere on this path)
                // rather than rotating to another account.
                return Err(mapped_upstream_error(status, upstream, auth).await);
            }
            FirstOutcome::Rotate(upstream) => {
                last_response = Some(upstream);
            }
            FirstOutcome::NeedRefresh(upstream) => {
                // Force-refresh the account's stored credential under its refresh
                // lock (see force_refresh_or_cooldown); a `token_env` account or a
                // refresh failure cools it down and rotates instead.
                let retry_credential =
                    match force_refresh_or_cooldown(&state, &route, account, &credential).await {
                        Some(credential) => credential,
                        None => {
                            last_response = Some(upstream);
                            continue;
                        }
                    };
                let retry = match http_send(
                    &state,
                    &route,
                    retry_credential,
                    session_id.as_deref(),
                    body.clone(),
                )
                .await
                {
                    Ok(response) => response,
                    Err(error @ crate::upstream_timeout::SendError::Timeout) => {
                        return Err(
                            error.into_adapter_error(|error| transport_error(error.to_string()))
                        );
                    }
                    Err(crate::upstream_timeout::SendError::Transport(error)) => {
                        state.accounts.cooldown(
                            &route.provider,
                            account,
                            Duration::from_secs(30),
                            "transport",
                        );
                        tracing::warn!(
                            provider = %route.provider,
                            account = %account.name,
                            error = %error.without_url(),
                            "ChatGPT OAuth refresh retry failed"
                        );
                        last_response = Some(upstream);
                        continue;
                    }
                };
                state
                    .accounts
                    .note_codex_quota(&route.provider, account, retry.headers());
                match classify_retry(&state, &route, account, retry) {
                    RetryOutcome::Relay(retry) => {
                        let retry_status = retry.status();
                        if retry_status.is_success() {
                            state.accounts.mark_healthy(&route.provider, account, true);
                            let input_tokens_estimate = take_estimate(&mut estimate_handle).await;
                            let response = relay_success(
                                &state,
                                retry,
                                turn.client_wants_stream,
                                turn.relay(&route),
                                input_tokens_estimate,
                            )
                            .await?;
                            let response = crate::adapters::with_admission(
                                with_account_header(response, &account.name),
                                admission,
                            );
                            // Surface the real status (issue #113) rather than a
                            // hardcoded `200` — see the relay arm above.
                            return Ok((response.status(), response));
                        }
                        return Err(mapped_upstream_error(retry_status, retry, auth).await);
                    }
                    RetryOutcome::Rotate(retry) => {
                        last_response = Some(retry);
                    }
                }
            }
        }
    }

    crate::metrics::record_pool_rotation(&route.provider, "exhausted");
    match last_response {
        Some(upstream) => {
            let status = upstream.status();
            Err(mapped_upstream_error(status, upstream, auth).await)
        }
        None => Err(transport_error(
            "all Codex OAuth accounts failed before receiving an upstream response".to_string(),
        )),
    }
}

/// Relay a successful upstream Responses answer to the client, choosing SSE
/// or a single JSON body per `client_wants_stream`. Thin wrapper shared by
/// every success arm in [`forward_chatgpt_oauth`] so each only differs in
/// which upstream response and account produced it (mirrors how the
/// single-account [`forward_http`] picks between [`stream_response`] and
/// [`json_response`]).
async fn relay_success(
    state: &AppState,
    upstream: reqwest::Response,
    client_wants_stream: bool,
    relay: RelayOptions,
    input_tokens_estimate: u64,
) -> Result<axum::response::Response, AdapterError> {
    if client_wants_stream {
        let keepalive = Duration::from_secs(state.config.server.sse_keepalive_seconds);
        Ok(stream_response(
            upstream,
            relay,
            input_tokens_estimate,
            keepalive,
        ))
    } else {
        json_response(upstream, relay).await
    }
}

/// Await the lazily-spawned HTTP-path tiktoken estimate handle exactly once,
/// mirroring `forward_http`'s inline `match estimate_handle { .. }`. Factored
/// out because the pool loop has two success arms (first attempt and
/// refresh retry) that must consume the same handle without awaiting it
/// twice — `JoinHandle` is not `Clone`, so `.take()` leaves a torn-down `None`
/// behind for whichever arm does not run. Non-streaming turns never seed
/// `message_start`, so `estimate_handle` is always `None` here already
/// (`forward`'s gate only produces `estimate_input`, and thus a spawned
/// handle, for streaming turns), which naturally yields `0` below.
async fn take_estimate(estimate_handle: &mut Option<tokio::task::JoinHandle<u64>>) -> u64 {
    match estimate_handle.take() {
        // Bounded like `forward_http`: the committed `message_start` must not
        // wait on the estimator (see `bounded_input_estimate`).
        Some(handle) => bounded_input_estimate(handle, std::time::Duration::from_secs(1)).await,
        None => 0,
    }
}

/// Inject `x-shunt-account` naming which pool account produced the response,
/// mirroring the Anthropic adapter's `relay_response`. Silently skipped if the
/// account name is not a valid header value — should never happen, since
/// account names are validated against `[a-z0-9-]+` at import time (see
/// `auth::codex::store::validate_account_name`).
pub(super) fn with_account_header(
    mut response: axum::response::Response,
    account_name: &str,
) -> axum::response::Response {
    if let Ok(value) = HeaderValue::from_str(account_name) {
        response.headers_mut().insert("x-shunt-account", value);
    }
    response
}

// --- Shared Codex/ChatGPT pool failover primitives ---------------------------
//
// These are the parts of the per-account failover machine that are identical
// between the translating outbound path ([`forward_chatgpt_oauth`], above) and
// the verbatim inbound passthrough (`responses::inbound::forward_codex_inbound`):
// cooldown timing, credential resolution, and force-refresh. Sharing them keeps
// the two paths from drifting and avoids duplicating the cooldown/refresh rules.

/// Cooldown for a rotate-worthy upstream status on the Codex pool: honor a 429's
/// `retry-after` (clamped to 1s..=1h), otherwise a flat 30s. Shared so the
/// translating and passthrough paths back off identically.
pub(super) fn rotate_cooldown(
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
) -> Duration {
    if status == StatusCode::TOO_MANY_REQUESTS {
        accounts::retry_after(headers)
            .unwrap_or(Duration::from_secs(60))
            .clamp(Duration::from_secs(1), Duration::from_secs(3600))
    } else {
        Duration::from_secs(30)
    }
}

/// Shared per-candidate prelude of the outbound pool and inbound passthrough
/// loops: storm-control admission for the candidate
/// ([`crate::accounts::AccountPool::admit_candidate`]), then credential
/// resolution ([`resolve_or_cooldown`]). `None` rotates to the next candidate —
/// the identity is either saturated or failed auth (already cooled down). The
/// returned guard (present when storm control is enabled) must be held for the
/// whole account attempt; on a relayed success move it into the response body
/// (`with_admission`) so the slot stays held until the stream finishes.
pub(super) async fn admit_and_resolve(
    state: &AppState,
    route: &Route,
    account: &AccountConfig,
    ramp_initial: Option<u32>,
    position: usize,
    candidates: usize,
) -> Option<(Option<accounts::AdmissionGuard>, Credential)> {
    let admission = state.accounts.admit_candidate(
        &route.provider,
        account,
        ramp_initial,
        position,
        candidates,
    )?;
    let credential = resolve_or_cooldown(state, route, account).await?;
    Some((admission, credential))
}

/// Resolve one Codex/ChatGPT OAuth account's credential. Valid stored tokens
/// return without synchronization; expired-token refreshes are single-flighted by
/// [`CodexAuthStore::get_valid_chatgpt`] using the credential path and keep that
/// auth-layer guard through atomic writeback. On failure the account is cooled
/// down for 5 minutes and logged, and `None` signals the caller to rotate to the
/// next account.
pub(super) async fn resolve_or_cooldown(
    state: &AppState,
    route: &Route,
    account: &AccountConfig,
) -> Option<Credential> {
    match resolve_chatgpt_account(account, &state.http_client).await {
        Ok(credential) => Some(credential),
        Err(error) => {
            state.accounts.cooldown(
                &route.provider,
                account,
                Duration::from_secs(5 * 60),
                "auth",
            );
            tracing::warn!(
                provider = %route.provider,
                account = %account.name,
                error = %error.message,
                "failed to resolve ChatGPT OAuth account"
            );
            None
        }
    }
}

pub(super) fn chatgpt_access_token(credential: &Credential) -> Option<&str> {
    match credential {
        Credential::ChatGptOAuth { access_token, .. } => Some(access_token),
        _ => None,
    }
}

/// Force-refresh one Codex/ChatGPT OAuth account's stored credential under its
/// refresh lock, returning the refreshed credential to retry with. A `token_env`
/// (static) account has nothing to refresh — unlike Claude, Codex's store never
/// encodes a non-refreshable "long-lived setup token" shape (see
/// auth/codex/store.rs), so the only static source is an explicit `token_env` —
/// and a refresh failure likewise cools the account down. Either case returns
/// `None` to signal the caller to rotate. The lock is released before the caller
/// retries upstream (never held across a send).
pub(super) async fn force_refresh_or_cooldown(
    state: &AppState,
    route: &Route,
    account: &AccountConfig,
    credential: &Credential,
) -> Option<Credential> {
    if account.token_env.is_some() {
        state.accounts.cooldown(
            &route.provider,
            account,
            Duration::from_secs(5 * 60),
            "auth",
        );
        tracing::warn!(
            provider = %route.provider,
            account = %account.name,
            "ChatGPT OAuth account returned 401 but its credential is not refreshable (token_env); cooling down"
        );
        return None;
    }

    let rejected_access_token = match chatgpt_access_token(credential) {
        Some(access_token) => access_token,
        None => {
            state.accounts.cooldown(
                &route.provider,
                account,
                Duration::from_secs(5 * 60),
                "auth",
            );
            tracing::warn!(
                provider = %route.provider,
                account = %account.name,
                "Codex pool account returned 401 with a non-ChatGPT credential; cooling down"
            );
            return None;
        }
    };

    let credentials_path = account
        .credentials
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| auth::codex::store::account_path(&account.name));
    let store = CodexAuthStore::new(credentials_path, state.http_client.clone());
    let refresh_lock = state.accounts.refresh_lock(&route.provider, account);
    let _guard = refresh_lock.lock().await;
    match store
        .force_refresh_if_access_token(rejected_access_token)
        .await
    {
        Ok(refreshed) => Some(Credential::ChatGptOAuth {
            access_token: refreshed.access_token,
            account_id: refreshed.account_id,
        }),
        Err(error) => {
            state.accounts.cooldown(
                &route.provider,
                account,
                Duration::from_secs(5 * 60),
                "auth",
            );
            tracing::warn!(
                provider = %route.provider,
                account = %account.name,
                error = %error.message,
                "failed to force-refresh ChatGPT OAuth account"
            );
            None
        }
    }
}

/// The classification of a **first-attempt** upstream response on the Codex pool.
/// The account-specific relay rendering (translate vs verbatim, and the
/// `mark_healthy`) stays with the caller; the shared cooldown/rotate bookkeeping
/// is applied here so the translating and passthrough paths classify identically.
pub(super) enum FirstOutcome {
    /// The account is fine — the caller marks it healthy and relays this response
    /// its own way.
    Relay(reqwest::Response),
    /// The account failed over (already cooled down); the caller stashes this
    /// response as the pool's last-seen and rotates.
    Rotate(reqwest::Response),
    /// A 401 — the caller should force-refresh and retry this account.
    NeedRefresh(reqwest::Response),
}

/// Classify a first-attempt upstream response, applying the shared rotate cooldown.
/// A `Relay` account is left for the caller to mark healthy so it can render the
/// response its own way (a translating path splits success vs a non-failover 4xx;
/// the passthrough relays verbatim).
pub(super) fn classify_first(
    state: &AppState,
    route: &Route,
    account: &AccountConfig,
    upstream: reqwest::Response,
) -> FirstOutcome {
    let status = upstream.status();
    match accounts::classify_codex(status, upstream.headers()) {
        FailoverAction::Relay => FirstOutcome::Relay(upstream),
        FailoverAction::Rotate => {
            let cooldown = rotate_cooldown(status, upstream.headers());
            state.accounts.cooldown(
                &route.provider,
                account,
                cooldown,
                accounts::rotation_reason(status, upstream.headers()),
            );
            tracing::warn!(
                provider = %route.provider,
                account = %account.name,
                status = %status,
                "codex pool account failed over; cooling down and rotating to the next account"
            );
            FirstOutcome::Rotate(upstream)
        }
        FailoverAction::RefreshRetry => FirstOutcome::NeedRefresh(upstream),
        FailoverAction::PauseSame => unreachable!("classify_codex never returns PauseSame"),
    }
}

/// The classification of a **refreshed-retry** upstream response on the Codex pool.
pub(super) enum RetryOutcome {
    /// The caller marks the account healthy and relays this refreshed response.
    Relay(reqwest::Response),
    /// Rotate (already cooled down); the caller stashes this as the pool's
    /// last-seen.
    Rotate(reqwest::Response),
}

/// Classify a refreshed-retry upstream response. A retry still rejected with 401
/// (the refresh succeeded but the credential is still bad) or otherwise
/// non-relayable cools the account down and rotates; only a relayable status is
/// handed back for the caller to render. `classify_codex` returns `RefreshRetry`
/// only for 401 (handled above) and never `PauseSame`, so only `Relay` and
/// `Rotate` are live — the others ride `Rotate`'s arm as a defensive no-op.
pub(super) fn classify_retry(
    state: &AppState,
    route: &Route,
    account: &AccountConfig,
    retry: reqwest::Response,
) -> RetryOutcome {
    let retry_status = retry.status();
    if retry_status == StatusCode::UNAUTHORIZED {
        state.accounts.cooldown(
            &route.provider,
            account,
            Duration::from_secs(5 * 60),
            "auth",
        );
        tracing::warn!(
            provider = %route.provider,
            account = %account.name,
            "codex pool account refreshed but upstream still rejected the new credential; cooling down and rotating"
        );
        return RetryOutcome::Rotate(retry);
    }
    match accounts::classify_codex(retry_status, retry.headers()) {
        FailoverAction::Relay => RetryOutcome::Relay(retry),
        FailoverAction::Rotate | FailoverAction::RefreshRetry => {
            let cooldown = rotate_cooldown(retry_status, retry.headers());
            state.accounts.cooldown(
                &route.provider,
                account,
                cooldown,
                accounts::rotation_reason(retry_status, retry.headers()),
            );
            tracing::warn!(
                provider = %route.provider,
                account = %account.name,
                status = %retry_status,
                "codex pool refresh retry did not succeed; rotating to the next account"
            );
            RetryOutcome::Rotate(retry)
        }
        FailoverAction::PauseSame => unreachable!("classify_codex never returns PauseSame"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use serde_json::json;

    use super::super::context::TurnOptions;
    use super::*;
    use crate::{
        accounts::{account_key, QuotaState, StoreFamily},
        config::{Config, PoolConfig},
        routing::{AdapterKind, Route},
    };
    use axum::http::StatusCode;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Serializes the env vars the pool-probe tests write; every test in this
    /// module that calls `std::env::set_var` must hold it for its whole body.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn pool_route() -> Route {
        Route {
            provider: "codex".to_string(),
            adapter: AdapterKind::Responses,
            model: "gpt-5.2-codex".to_string(),
            upstream_model: "gpt-5.2-codex".to_string(),
            effort: None,
            service_tier: None,
        }
    }

    fn pool_account(name: &str, token_env: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_string(),
            token_env: Some(token_env.to_string()),
            ..Default::default()
        }
    }

    fn pool_state(base_url: String) -> AppState {
        let mut config = Config::default();
        config.providers.get_mut("codex").unwrap().base_url = base_url;
        AppState::new(config, reqwest::Client::new()).unwrap()
    }

    /// A minimal unverified JWT carrying only the ChatGPT account-id claim in
    /// the nested shape `codex::auth::jwt_account_id` reads — enough for
    /// `resolve_chatgpt_account`'s `token_env` path, which decodes the payload
    /// and never checks a signature or expiry.
    fn probe_token(account_id: &str) -> String {
        let payload = URL_SAFE_NO_PAD.encode(
            json!({"https://api.openai.com/auth": {"chatgpt_account_id": account_id}}).to_string(),
        );
        format!("e30.{payload}.e30")
    }

    fn pool_turn(accounts: Vec<AccountConfig>, stream: bool) -> PoolForward {
        PoolForward {
            pool_key: None,
            session_id: None,
            upstream_body: std::sync::Arc::new(json!({"input": []})),
            accounts_config: accounts,
            turn: TurnOptions {
                client_wants_stream: stream,
                thinking_enabled: false,
                tool_search_native: false,
            },
            estimate_input: None,
        }
    }

    /// The pool's streaming arm commits the synthetic start before any
    /// upstream byte — the client's stall watchdog is fed while the account
    /// loop (admission, credential resolution, the send itself) still runs.
    #[tokio::test]
    async fn pool_streaming_arm_commits_synthetic_start_before_any_upstream_byte() {
        use futures_util::StreamExt;
        let _env = ENV_LOCK.lock().await;
        let _token_a =
            crate::auth::shared::EnvVarGuard::set("SHUNT_POOL_PROBE_A", probe_token("acc-a"));
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(30))
                    .set_body_string(
                        "event: response.created\ndata: {\"response\":{\"id\":\"resp_1\"}}\n\n\
                         event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
                    ),
            )
            .mount(&server)
            .await;
        let accounts = vec![pool_account("pool-probe-a", "SHUNT_POOL_PROBE_A")];
        let state = pool_state(server.uri());
        let (status, response) =
            forward_chatgpt_oauth(state, pool_route(), pool_turn(accounts, true))
                .await
                .expect("streaming pool turn builds the response without upstream headers");
        assert_eq!(status, StatusCode::OK);
        let mut body = response.into_body().into_data_stream();
        let first = tokio::time::timeout(Duration::from_secs(2), body.next())
            .await
            .expect("first chunk arrives while the upstream is silent")
            .expect("stream yields")
            .expect("chunk is ok");
        let text = String::from_utf8(first.to_vec()).expect("chunk is utf8");
        assert!(
            text.starts_with("event: message_start\ndata: "),
            "got: {text}"
        );
        assert!(text.contains("\"id\":\"msg_"), "synthetic id, got: {text}");
    }

    /// A transport-level rotation still emits exactly one message_start, and
    /// the relayed content flows after it.
    #[tokio::test]
    async fn pool_streaming_arm_rotates_and_relays_from_the_second_account() {
        let _env = ENV_LOCK.lock().await;
        let _token_a =
            crate::auth::shared::EnvVarGuard::set("SHUNT_POOL_PROBE_A", probe_token("acc-a"));
        let _token_b =
            crate::auth::shared::EnvVarGuard::set("SHUNT_POOL_PROBE_B", probe_token("acc-b"));
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "event: response.created\ndata: {\"response\":{\"id\":\"resp_2\"}}\n\n\
                 event: response.output_text.delta\ndata: {\"delta\":\"hi\"}\n\n\
                 event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
            ))
            .mount(&server)
            .await;
        let accounts = vec![
            pool_account("pool-probe-a", "SHUNT_POOL_PROBE_A"),
            pool_account("pool-probe-b", "SHUNT_POOL_PROBE_B"),
        ];
        let state = pool_state(server.uri());
        let (_, response) = forward_chatgpt_oauth(state, pool_route(), pool_turn(accounts, true))
            .await
            .expect("pool turn succeeds on the second account");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body is readable");
        let text = String::from_utf8_lossy(&bytes);
        assert_eq!(
            text.matches("event: message_start").count(),
            1,
            "got: {text}"
        );
        assert!(text.contains("\"id\":\"msg_"), "synthetic id, got: {text}");
        assert!(text.contains("\"text\":\"hi\""), "got: {text}");
        assert_eq!(
            server
                .received_requests()
                .await
                .expect("mock records requests")
                .len(),
            2,
            "one rotation then one success"
        );
    }

    /// Pool exhaustion surfaces as one terminal SSE error event on the
    /// committed stream — never a synthesized completion.
    #[tokio::test]
    async fn pool_streaming_arm_emits_error_event_on_exhaustion() {
        let _env = ENV_LOCK.lock().await;
        let _token_a =
            crate::auth::shared::EnvVarGuard::set("SHUNT_POOL_PROBE_A", probe_token("acc-a"));
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let accounts = vec![pool_account("pool-probe-a", "SHUNT_POOL_PROBE_A")];
        let state = pool_state(server.uri());
        let (status, response) =
            forward_chatgpt_oauth(state, pool_route(), pool_turn(accounts, true))
                .await
                .expect("pool turn commits and reports the failure in-stream");
        assert_eq!(status, StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body is readable");
        let text = String::from_utf8_lossy(&bytes);
        assert_eq!(text.matches("event: message_start").count(), 1);
        assert!(text.contains("event: error\ndata: "), "got: {text}");
        assert!(
            text.contains("\"type\":\"api_error\""),
            "a mapped 500 exhaustion carries the api_error envelope, got: {text}"
        );
        assert!(
            !text.contains("event: message_stop"),
            "no synthesized completion after an error, got: {text}"
        );
    }

    /// The non-streaming arm keeps its pre-commit status relay: a 429 stays an
    /// error response with the real status, not an early-committed stream.
    #[tokio::test]
    async fn pool_non_streaming_arm_keeps_the_status_relay() {
        let _env = ENV_LOCK.lock().await;
        let _token_a =
            crate::auth::shared::EnvVarGuard::set("SHUNT_POOL_PROBE_A", probe_token("acc-a"));
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string("{}"))
            .mount(&server)
            .await;
        let accounts = vec![pool_account("pool-probe-a", "SHUNT_POOL_PROBE_A")];
        let state = pool_state(server.uri());
        let error = forward_chatgpt_oauth(state, pool_route(), pool_turn(accounts, false))
            .await
            .expect_err("non-streaming 429 stays an error response");
        assert_eq!(error.response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn websocket_gate_controls_reprobe_selection_and_stamp() {
        let mut config = Config::default();
        config.server.pool = Some(PoolConfig {
            default_threshold: Some(0.5),
            reprobe_seconds: Some(60),
            ..Default::default()
        });
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        let mut a = AccountConfig {
            name: "codex-a".to_string(),
            ..Default::default()
        };
        a.store_family = Some(StoreFamily::Chatgpt);
        let mut b = AccountConfig {
            name: "codex-b".to_string(),
            ..Default::default()
        };
        b.store_family = Some(StoreFamily::Chatgpt);
        let accounts = vec![a, b];
        let session = "websocket-reprobe-gate";
        let initial = state.accounts.select_order(
            "codex",
            &accounts,
            Some(session),
            Some("test-model"),
            state.config.server.pool.as_ref(),
        );
        let stale = initial[0];
        let other = initial[1];
        let observed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 61;
        state.accounts.import_quotas([(
            account_key("codex", &accounts[stale]),
            QuotaState {
                utilization_5h: Some(0.9),
                observed_at_5h: Some(observed_at),
                ..Default::default()
            },
        )]);

        let ws_order = select_pool_order(
            &state,
            "codex",
            &accounts,
            Some(session),
            "test-model",
            true,
        );
        assert_eq!(
            ws_order,
            vec![other, stale],
            "WebSocket-enabled selection must not promote a stale account"
        );
        assert_eq!(
            state
                .accounts
                .last_probe_at_for_test("codex", &accounts[stale]),
            None,
            "WebSocket-enabled selection must not consume the shared probe interval"
        );

        let (http_order, mut reservation) = state.accounts.select_order_deferred(
            "codex",
            &accounts,
            Some(session),
            Some("test-model"),
            state.config.server.pool.as_ref(),
        );
        assert_eq!(
            http_order,
            vec![stale, other],
            "HTTP selection must retain stale-account re-probing"
        );
        assert!(
            reservation.is_some(),
            "HTTP selection must carry a reservation"
        );
        assert_eq!(
            state
                .accounts
                .last_probe_at_for_test("codex", &accounts[stale]),
            None,
            "HTTP selection must defer the shared probe stamp until dispatch"
        );
        reservation.as_mut().unwrap().commit();
        assert!(state
            .accounts
            .last_probe_at_for_test("codex", &accounts[stale])
            .is_some());
    }

    #[test]
    fn reprobe_helpers_ignore_mismatching_indices_and_cleanup_reservations() {
        let provider = "pool-reprobe-helper-mismatch";
        let mut config = Config::default();
        config.server.pool = Some(PoolConfig {
            default_threshold: Some(0.5),
            reprobe_seconds: Some(60),
            ..Default::default()
        });
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        let mut stale = AccountConfig {
            name: "pool-helper-stale".to_string(),
            ..Default::default()
        };
        stale.store_family = Some(StoreFamily::Chatgpt);
        let mut healthy = AccountConfig {
            name: "pool-helper-healthy".to_string(),
            ..Default::default()
        };
        healthy.store_family = Some(StoreFamily::Chatgpt);
        let accounts = vec![stale, healthy];
        let session = "pool-reprobe-helper-mismatch-session";
        let initial = state.accounts.select_order(
            provider,
            &accounts,
            Some(session),
            Some("test-model"),
            state.config.server.pool.as_ref(),
        );
        let stale_index = initial[0];
        state.accounts.import_quotas([(
            account_key(provider, &accounts[stale_index]),
            QuotaState {
                utilization_5h: Some(0.9),
                observed_at_5h: Some(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs()
                        - 61,
                ),
                ..Default::default()
            },
        )]);
        let wrong_index = 1 - stale_index;

        let (_, mut commit_reservation) = state.accounts.select_order_deferred(
            provider,
            &accounts,
            Some(session),
            Some("test-model"),
            state.config.server.pool.as_ref(),
        );
        let before = crate::metrics::pool_reprobe_count_for_tests(provider);
        commit_reprobe_for_account(&mut commit_reservation, wrong_index);
        assert_eq!(
            state
                .accounts
                .last_probe_at_for_test(provider, &accounts[stale_index]),
            None,
            "a mismatching index must not commit another account's reservation"
        );
        assert_eq!(
            crate::metrics::pool_reprobe_count_for_tests(provider),
            before
        );
        commit_reprobe_for_account(&mut commit_reservation, stale_index);
        assert!(state
            .accounts
            .last_probe_at_for_test(provider, &accounts[stale_index])
            .is_some());
        assert_eq!(
            crate::metrics::pool_reprobe_count_for_tests(provider),
            before + 1
        );

        let cancel_provider = "pool-reprobe-helper-cancel";
        let session = "pool-reprobe-helper-cancel-session";
        let cancel_pool = state.accounts.clone();
        let initial = cancel_pool.select_order(
            cancel_provider,
            &accounts,
            Some(session),
            Some("test-model"),
            state.config.server.pool.as_ref(),
        );
        let stale_index = initial[0];
        cancel_pool.import_quotas([(
            account_key(cancel_provider, &accounts[stale_index]),
            QuotaState {
                utilization_5h: Some(0.9),
                observed_at_5h: Some(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs()
                        - 61,
                ),
                ..Default::default()
            },
        )]);
        let wrong_index = 1 - stale_index;
        let (_, mut cancel_reservation) = cancel_pool.select_order_deferred(
            cancel_provider,
            &accounts,
            Some(session),
            Some("test-model"),
            state.config.server.pool.as_ref(),
        );
        cancel_reprobe_for_account(&mut cancel_reservation, wrong_index);
        let (_, blocked_reservation) = cancel_pool.select_order_deferred(
            cancel_provider,
            &accounts,
            Some(session),
            Some("test-model"),
            state.config.server.pool.as_ref(),
        );
        assert!(
            blocked_reservation.is_none(),
            "a mismatching cancel must leave the original reservation pending"
        );
        cancel_reprobe_for_account(&mut cancel_reservation, stale_index);
        assert_eq!(
            cancel_pool.last_probe_at_for_test(cancel_provider, &accounts[stale_index]),
            None
        );
        let (_, retry_reservation) = cancel_pool.select_order_deferred(
            cancel_provider,
            &accounts,
            Some(session),
            Some("test-model"),
            state.config.server.pool.as_ref(),
        );
        assert!(
            retry_reservation.is_some(),
            "matching cancel must clean up the pending reservation"
        );
        drop(retry_reservation);
    }

    #[tokio::test]
    async fn valid_token_resolution_does_not_wait_for_account_refresh_lock() {
        let dir = std::env::temp_dir().join(format!(
            "shunt-responses-pool-valid-token-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        let future_exp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3_600;
        let payload = json!({
            "exp": future_exp,
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-valid"}
        });
        let access_token = format!(
            "x.{}.y",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        );
        std::fs::write(
            &path,
            json!({
                "auth_mode": "ChatGPT",
                "tokens": {
                    "access_token": access_token.clone(),
                    "refresh_token": "unused-refresh-token"
                }
            })
            .to_string(),
        )
        .unwrap();

        let account = AccountConfig {
            name: "valid-token".to_string(),
            credentials: Some(path.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let route = Route {
            provider: "codex".to_string(),
            adapter: AdapterKind::Responses,
            model: "test-model".to_string(),
            upstream_model: "test-model".to_string(),
            effort: None,
            service_tier: None,
        };
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();

        // Deliberately hold the account-pool lock. Valid-token resolution must
        // bypass it; only an auth-layer refresh needs synchronization.
        let refresh_lock = state.accounts.refresh_lock(&route.provider, &account);
        let _guard = refresh_lock.lock().await;
        let credential = tokio::time::timeout(
            Duration::from_secs(3),
            resolve_or_cooldown(&state, &route, &account),
        )
        .await
        .expect("valid-token resolution must not wait for the account-pool refresh lock")
        .expect("valid stored token should resolve");

        assert_eq!(
            chatgpt_access_token(&credential),
            Some(access_token.as_str())
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A state whose provider `name` is a clone of the built-in codex
    /// provider with `base_url` repointed — the per-test provider names keep
    /// the global test sample store free of cross-test interference.
    fn pool_state_with_provider(name: &str, base_url: String) -> (AppState, Route) {
        let mut config = Config::default();
        config.providers.insert(
            name.to_string(),
            config
                .providers
                .get("codex")
                .expect("codex provider is built in")
                .clone(),
        );
        config
            .providers
            .get_mut(name)
            .expect("just inserted")
            .base_url = base_url;
        let mut route = pool_route();
        route.provider = name.to_string();
        (
            AppState::new(config, reqwest::Client::new()).unwrap(),
            route,
        )
    }

    /// The committed pool stream records the request sample at
    /// classification: one `200` with the real upstream latency once an
    /// account relays, never the near-zero dispatch/commit time.
    #[tokio::test]
    async fn pool_stream_records_the_sample_at_classification() {
        let _env = ENV_LOCK.lock().await;
        let _token = crate::auth::shared::EnvVarGuard::set(
            "SHUNT_POOL_METRICS_PROBE",
            probe_token("acc-metrics"),
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(100))
                    .set_body_string(
                        "event: response.created\ndata: {\"response\":{\"id\":\"resp_1\"}}\n\n\
                         event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
                    ),
            )
            .mount(&server)
            .await;
        let accounts = vec![pool_account(
            "pool-metrics-probe-a",
            "SHUNT_POOL_METRICS_PROBE",
        )];
        let (state, route) = pool_state_with_provider("pool-metrics-probe", server.uri());
        let (_, response) = forward_chatgpt_oauth(state, route, pool_turn(accounts, true))
            .await
            .expect("pool turn commits and relays");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body is readable");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("event: message_stop"), "got: {text}");
        let (count, latencies) = crate::metrics::proxied_request_samples_for_tests(
            "pool-metrics-probe",
            "gpt-5.2-codex",
            200,
        );
        assert_eq!(count, 1, "exactly one sample at classification");
        assert!(
            latencies.iter().all(|latency| *latency >= 50.0),
            "the sample covers the upstream round-trip, not the near-zero commit, got {latencies:?}"
        );
    }

    /// Pool exhaustion records the classified terminal status, not the
    /// committed 200.
    #[tokio::test]
    async fn pool_stream_records_the_terminal_status_on_exhaustion() {
        let _env = ENV_LOCK.lock().await;
        let _token = crate::auth::shared::EnvVarGuard::set(
            "SHUNT_POOL_METRICS_FAIL_PROBE",
            probe_token("acc-metrics-fail"),
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let accounts = vec![pool_account(
            "pool-metrics-fail-probe-a",
            "SHUNT_POOL_METRICS_FAIL_PROBE",
        )];
        let (state, route) = pool_state_with_provider("pool-metrics-fail-probe", server.uri());
        let (_, response) = forward_chatgpt_oauth(state, route, pool_turn(accounts, true))
            .await
            .expect("pool turn commits and reports the failure in-stream");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body is readable");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("event: error"), "got: {text}");
        let (count, _) = crate::metrics::proxied_request_samples_for_tests(
            "pool-metrics-fail-probe",
            "gpt-5.2-codex",
            500,
        );
        assert_eq!(
            count, 1,
            "the classified exhaustion records its real status"
        );
    }

    /// The chain owns its attempt's sample: a pool stream built for the
    /// chain (`record_metrics: false`) must record nothing itself, or the
    /// chain's per-attempt record would double-count.
    #[tokio::test]
    async fn a_chain_pool_stream_leaves_the_sample_to_the_chain() {
        let _env = ENV_LOCK.lock().await;
        let _token = crate::auth::shared::EnvVarGuard::set(
            "SHUNT_POOL_METRICS_CHAIN_PROBE",
            probe_token("acc-metrics-chain"),
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "event: response.created\ndata: {\"response\":{\"id\":\"resp_1\"}}\n\n\
                 event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
            ))
            .mount(&server)
            .await;
        let accounts = vec![pool_account(
            "pool-metrics-chain-probe-a",
            "SHUNT_POOL_METRICS_CHAIN_PROBE",
        )];
        let (state, route) = pool_state_with_provider("pool-metrics-chain-probe", server.uri());
        let (order, reprobe) = state.accounts.select_order_deferred(
            &route.provider,
            &accounts,
            None,
            Some(route.upstream_model.as_str()),
            state.config.server.pool.as_ref(),
        );
        let events = pool_events_stream(PoolStreamContext {
            state: state.clone(),
            route: route.clone(),
            auth: AuthMode::ChatgptOauth,
            session_id: None,
            upstream_body: std::sync::Arc::new(json!({"input": []})),
            accounts_config: std::sync::Arc::new(accounts),
            order,
            reprobe,
            ramp_initial: state.config.storm_ramp_initial(),
            record_metrics: false,
            started_at: None,
        });
        use futures_util::StreamExt;
        let collected: Vec<_> = events.collect().await;
        assert!(!collected.is_empty(), "the pool relays the account's turn");
        let (count, _) = crate::metrics::proxied_request_samples_for_tests(
            "pool-metrics-chain-probe",
            "gpt-5.2-codex",
            200,
        );
        assert_eq!(count, 0, "the chain records its attempt's sample");
    }

    /// The TTFB timeout on the committed pool stream classifies as 504 and
    /// records the sample.
    #[tokio::test]
    async fn pool_stream_records_the_ttfb_timeout_status() {
        let _env = ENV_LOCK.lock().await;
        let _token = crate::auth::shared::EnvVarGuard::set(
            "SHUNT_POOL_METRICS_TTFB_PROBE",
            probe_token("acc-metrics-ttfb"),
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
            .mount(&server)
            .await;
        let accounts = vec![pool_account(
            "pool-metrics-ttfb-probe-a",
            "SHUNT_POOL_METRICS_TTFB_PROBE",
        )];
        let mut config = Config::default();
        config.providers.insert(
            "pool-metrics-ttfb-probe".to_string(),
            config
                .providers
                .get("codex")
                .expect("codex provider is built in")
                .clone(),
        );
        config
            .providers
            .get_mut("pool-metrics-ttfb-probe")
            .expect("just inserted")
            .base_url = server.uri();
        config.server.timeouts.upstream_ttfb_ms = 100;
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        let mut route = pool_route();
        route.provider = "pool-metrics-ttfb-probe".to_string();
        let (_, response) = forward_chatgpt_oauth(state, route, pool_turn(accounts, true))
            .await
            .expect("pool turn commits and reports the timeout in-stream");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body is readable");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("event: error"), "got: {text}");
        let (count, _) = crate::metrics::proxied_request_samples_for_tests(
            "pool-metrics-ttfb-probe",
            "gpt-5.2-codex",
            504,
        );
        assert_eq!(count, 1, "the TTFB timeout records its 504");
    }

    /// Transport exhaustion (every account failed before a response) records
    /// the classified 502 from the exhaustion arm, never a committed 200.
    #[tokio::test]
    async fn pool_stream_records_the_transport_exhaustion_status() {
        let _env = ENV_LOCK.lock().await;
        let _token = crate::auth::shared::EnvVarGuard::set(
            "SHUNT_POOL_METRICS_REFUSED_PROBE",
            probe_token("acc-metrics-refused"),
        );
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )
        .unwrap();
        socket
            .bind(
                &"127.0.0.1:0"
                    .parse::<std::net::SocketAddr>()
                    .unwrap()
                    .into(),
            )
            .unwrap();
        let port = socket.local_addr().unwrap().as_socket().unwrap().port();
        let accounts = vec![pool_account(
            "pool-metrics-refused-probe-a",
            "SHUNT_POOL_METRICS_REFUSED_PROBE",
        )];
        let mut config = Config::default();
        config.providers.insert(
            "pool-metrics-refused-probe".to_string(),
            config
                .providers
                .get("codex")
                .expect("codex provider is built in")
                .clone(),
        );
        let provider = config
            .providers
            .get_mut("pool-metrics-refused-probe")
            .expect("just inserted");
        provider.base_url = format!("http://127.0.0.1:{port}");
        provider.retry = crate::config::RetryConfig {
            max_retries: 0,
            ..Default::default()
        };
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        let mut route = pool_route();
        route.provider = "pool-metrics-refused-probe".to_string();
        let (_, response) = forward_chatgpt_oauth(state, route, pool_turn(accounts, true))
            .await
            .expect("pool turn commits and reports the exhaustion in-stream");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body is readable");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("event: error"), "got: {text}");
        let (count, _) = crate::metrics::proxied_request_samples_for_tests(
            "pool-metrics-refused-probe",
            "gpt-5.2-codex",
            502,
        );
        assert_eq!(count, 1, "transport exhaustion records its 502");
        drop(socket);
    }

    /// A scan failure inside the committed pool stream classifies as 502 and
    /// records the sample — the committed response must not leave the
    /// request unsampled.
    #[tokio::test]
    async fn pool_or_single_records_the_scan_failure_status() {
        let (state, route) =
            pool_state_with_provider("pool-metrics-scan-probe", "http://127.0.0.1:1".to_string());
        let single = HttpSendContext {
            state: state.clone(),
            route: route.clone(),
            policy: crate::retry::RetryPolicy::DISABLED,
            credential: None,
            session_id: None,
            upstream_body: std::sync::Arc::new(json!({"input": []})),
            auth: AuthMode::ChatgptOauth,
            codex_quota_account: None,
        };
        let events = pool_or_single_events(
            Box::pin(async { Err("scan failed".to_string()) }),
            state.clone(),
            route.clone(),
            None,
            std::sync::Arc::new(json!({"input": []})),
            single,
            CredentialSource::Resolved(Credential::ApiKey {
                value: "probe".to_string(),
                header: crate::config::ApiKeyHeader::Bearer,
            }),
        );
        use futures_util::StreamExt;
        let collected: Vec<_> = events.collect().await;
        assert_eq!(collected.len(), 1, "one terminal item");
        let (count, _) = crate::metrics::proxied_request_samples_for_tests(
            "pool-metrics-scan-probe",
            "gpt-5.2-codex",
            502,
        );
        assert_eq!(count, 1, "the scan failure records its 502");
    }

    /// An all-disabled pool classifies as 502 and records the sample.
    #[tokio::test]
    async fn pool_or_single_records_the_all_disabled_status() {
        let (state, route) = pool_state_with_provider(
            "pool-metrics-disabled-probe",
            "http://127.0.0.1:1".to_string(),
        );
        let single = HttpSendContext {
            state: state.clone(),
            route: route.clone(),
            policy: crate::retry::RetryPolicy::DISABLED,
            credential: None,
            session_id: None,
            upstream_body: std::sync::Arc::new(json!({"input": []})),
            auth: AuthMode::ChatgptOauth,
            codex_quota_account: None,
        };
        let accounts = vec![AccountConfig {
            name: "disabled-metrics-a".to_string(),
            disabled: true,
            ..Default::default()
        }];
        let events = pool_or_single_events(
            Box::pin(async { Ok(accounts) }),
            state.clone(),
            route.clone(),
            None,
            std::sync::Arc::new(json!({"input": []})),
            single,
            CredentialSource::Resolved(Credential::ApiKey {
                value: "probe".to_string(),
                header: crate::config::ApiKeyHeader::Bearer,
            }),
        );
        use futures_util::StreamExt;
        let collected: Vec<_> = events.collect().await;
        assert_eq!(collected.len(), 1, "one terminal item");
        let (count, _) = crate::metrics::proxied_request_samples_for_tests(
            "pool-metrics-disabled-probe",
            "gpt-5.2-codex",
            502,
        );
        assert_eq!(count, 1, "the all-disabled pool records its 502");
    }

    /// A slow successful scan is inside the committed sample: the pre-scan
    /// start seeds the single-credential producer, so `shunt.latency` keeps
    /// the dispatch-to-classification span the pre-commit loop records.
    #[tokio::test]
    async fn pool_or_single_carries_the_scan_time_into_the_single_sample() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "event: response.created\ndata: {\"response\":{\"id\":\"resp_1\"}}\n\n\
                 event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
            ))
            .mount(&server)
            .await;
        let (state, route) =
            pool_state_with_provider("pool-metrics-scan-latency-probe", server.uri());
        let single = HttpSendContext {
            state: state.clone(),
            route: route.clone(),
            policy: crate::retry::RetryPolicy::DISABLED,
            credential: None,
            session_id: None,
            upstream_body: std::sync::Arc::new(json!({"input": []})),
            auth: AuthMode::ApiKey,
            codex_quota_account: None,
        };
        let events = pool_or_single_events(
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                Ok(vec![])
            }),
            state.clone(),
            route.clone(),
            None,
            std::sync::Arc::new(json!({"input": []})),
            single,
            CredentialSource::Resolved(Credential::ApiKey {
                value: "probe".to_string(),
                header: crate::config::ApiKeyHeader::Bearer,
            }),
        );
        use futures_util::StreamExt;
        let collected: Vec<_> = events.collect().await;
        assert!(!collected.is_empty(), "the turn relays");
        let (count, latencies) = crate::metrics::proxied_request_samples_for_tests(
            "pool-metrics-scan-latency-probe",
            "gpt-5.2-codex",
            200,
        );
        assert_eq!(count, 1, "one committed sample");
        assert!(
            latencies[0] >= 150.0,
            "the sample must include the scan: {}ms",
            latencies[0]
        );
    }

    /// The pool producer's sample starts at the pre-scan instant too: the
    /// committed pool sample covers the scan exactly like the chain's
    /// per-attempt record.
    #[tokio::test]
    async fn pool_or_single_carries_the_scan_time_into_the_pool_sample() {
        let _env = ENV_LOCK.lock().await;
        let _token = crate::auth::shared::EnvVarGuard::set(
            "SHUNT_POOL_SCAN_LATENCY_POOL_PROBE",
            probe_token("pool-metrics-scan-latency-pool"),
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "event: response.created\ndata: {\"response\":{\"id\":\"resp_1\"}}\n\n\
                 event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
            ))
            .mount(&server)
            .await;
        let (state, route) =
            pool_state_with_provider("pool-metrics-scan-latency-pool-probe", server.uri());
        let single = HttpSendContext {
            state: state.clone(),
            route: route.clone(),
            policy: crate::retry::RetryPolicy::DISABLED,
            credential: None,
            session_id: None,
            upstream_body: std::sync::Arc::new(json!({"input": []})),
            auth: AuthMode::ChatgptOauth,
            codex_quota_account: None,
        };
        let accounts = vec![pool_account(
            "pool-metrics-scan-latency-pool-a",
            "SHUNT_POOL_SCAN_LATENCY_POOL_PROBE",
        )];
        let events = pool_or_single_events(
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                Ok(accounts)
            }),
            state.clone(),
            route.clone(),
            None,
            std::sync::Arc::new(json!({"input": []})),
            single,
            CredentialSource::Resolved(Credential::ApiKey {
                value: "probe".to_string(),
                header: crate::config::ApiKeyHeader::Bearer,
            }),
        );
        use futures_util::StreamExt;
        let collected: Vec<_> = events.collect().await;
        assert!(!collected.is_empty(), "the pool relays the account's turn");
        let (count, latencies) = crate::metrics::proxied_request_samples_for_tests(
            "pool-metrics-scan-latency-pool-probe",
            "gpt-5.2-codex",
            200,
        );
        assert_eq!(count, 1, "one committed sample");
        assert!(
            latencies[0] >= 150.0,
            "the sample must include the scan: {}ms",
            latencies[0]
        );
    }
}
