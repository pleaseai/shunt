pub mod codex_continuation;
pub mod codex_ws;

mod body;
mod context;
mod early_stream;
mod error;
mod http;
pub(crate) mod inbound;
mod inbound_routed;
mod pool;
mod sse_parse;
// `pub(crate)` (not private): the chain (`proxy::chain_stream`) reuses the
// post-terminal drain for an Anthropic-kind winner's raw relay, so both
// winner kinds end the outward stream at the terminal frame under the same
// budget and the same pooling rule.
pub(crate) use sse_parse::spawn_terminal_drain;
// `pub(crate)` (not private): `crate::auth::codex::usage` reuses `CODEX_USER_AGENT`/
// `CODEX_CLIENT_VERSION` for the wham/usage poller so the CLI identity headers on
// that endpoint can never drift from the ones the Responses adapter itself sends.
pub(crate) mod request;
mod websocket;
mod ws_stream;

use std::sync::Arc;

use axum::http::{HeaderMap, StatusCode, Uri};
use serde_json::Value;

use crate::{
    adapters::{Adapter, AdapterError, AdapterFuture},
    auth::{self, resolve_credential, Credential},
    config::{AuthMode, CountTokens},
    model::responses::translate_request_value,
    request::RequestBody,
    routing::Route,
    server::AppState,
};

use futures_util::future::{BoxFuture, Shared};
use futures_util::{FutureExt, TryStreamExt};

use self::context::{CredentialSource, ForwardOptions, PoolForward, TurnOptions};
pub(crate) use self::early_stream::InStreamMetrics;
use self::early_stream::{
    parsed_events, pooled_first_poll, relay_build, send_classified, send_classified_with_estimate,
    translated_stream, HttpSendContext, PoolFirstPoll, PoolItem, SendClassified,
};
use self::error::{adapter_error_envelope, own_error, transport_error};
use self::http::forward_http;
pub(crate) use self::inbound::forward_codex_inbound;
pub(crate) use self::inbound_routed::forward_codex_routed;
use self::pool::{
    forward_chatgpt_oauth, forward_chatgpt_oauth_stream, pool_events_stream, PoolForwardStream,
    PoolStreamContext,
};
use self::sse_parse::pool_relay_build;
use self::websocket::forward_websocket;

pub struct ResponsesAdapter;

impl Adapter for ResponsesAdapter {
    fn forward<'a>(
        &'a self,
        state: AppState,
        route: Route,
        _uri: &'a Uri,
        headers: &'a HeaderMap,
        body: RequestBody,
        // Honoured only on the non-streaming path, which is the one that
        // buffers a whole upstream reply — and the one every internal call
        // takes, since `routing::serve` forces `stream` off. A streaming turn
        // relays instead of buffering, so its bound falls to that collector.
        response_byte_cap: Option<usize>,
    ) -> AdapterFuture<'a> {
        // The session id keys the websocket connection pool (issue #32) so turns
        // of one Claude Code conversation reuse a live connection. Keep an owned
        // value because the adapter future may outlive the borrowed header map.
        let session_id = headers
            .get("x-claude-code-session-id")
            .and_then(|value| value.to_str().ok())
            .filter(|session_id| !session_id.is_empty());
        let pool_key = session_id.map(|session_id| {
            headers
                .get("x-shunt-inbound-client")
                .and_then(|value| value.to_str().ok())
                .map_or_else(
                    || session_id.to_string(),
                    |client| format!("{client}:{session_id}"),
                )
        });
        Box::pin(async move {
            forward(
                state,
                route,
                pool_key,
                session_id.map(str::to_string),
                body,
                response_byte_cap,
            )
            .await
        })
    }
}

async fn forward(
    state: AppState,
    route: Route,
    pool_key: Option<String>,
    session_id: Option<String>,
    body: RequestBody,
    response_byte_cap: Option<usize>,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let request_json = body.json();
    // The effective conversation id: the inbound session header when present,
    // else the `metadata.user_id` session — the id the upstream session-id
    // headers AND the body prompt_cache_key must carry. A metadata-only client
    // still gets the affinity headers, without which the backend caches
    // nothing (measured 2026-09-20). The connection-pool key stays
    // header-derived (see `ResponsesAdapter::forward`); the account-pool
    // sticky key follows the effective id, so sessionless turns now pin per
    // conversation instead of rotating (the inbound endpoint's sticky-key
    // rationale).
    let session_id = session_id
        .or_else(|| crate::model::responses_request::effective_session_id(request_json, None));
    let client_wants_stream = request_json
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // Gates reasoning round-tripping (see model/responses.rs): surface thinking
    // blocks only when the client asked for extended thinking, since that is what
    // makes Claude Code echo them back on the next turn.
    let thinking_enabled = request_json
        .pointer("/thinking/type")
        .and_then(Value::as_str)
        == Some("enabled");
    let flavor = state.config.responses_flavor(&route.provider);
    // Native client-executed tool_search (issue #82) is used when the
    // provider/flavor/model gates pass — auto-on for a known-good host
    // (stock OpenAI or the ChatGPT/Codex backend), explicit `tool_search`
    // otherwise decides (issue #289); the #43 progressive-reveal shim is
    // used everywhere else.
    let tool_search_native = state
        .config
        .native_tool_search(&route.provider, &route.upstream_model);
    tracing::debug!(
        provider = %route.provider,
        upstream_model = %route.upstream_model,
        tool_search_native,
        "resolved tool_search protocol"
    );
    // The Responses API has no `stop` parameter (Chat Completions does; Responses
    // does not), so `stop_sequences` is emulated gateway-side in the
    // Responses->Anthropic SSE translation rather than forwarded upstream
    // (issue #605). Kept in request order: ties on the match position break by it.
    let stop_sequences: Vec<String> = request_json
        .get("stop_sequences")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .filter(|sequence| !sequence.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    // Seed message_start's usage.input_tokens with a local tiktoken estimate of
    // the (already-parsed) request so Claude Code's per-subagent progress
    // tracker — which reads that first snapshot and never re-reads the merged
    // total — shows a live context figure for codex subagents instead of a stuck
    // 0. The Responses API only reports real usage at response.completed, by
    // which point message_start is long sent; the accurate total still lands in
    // the terminal message_delta. Gated on the provider's local-counting opt-in
    // (the same CountTokens knob as the count_tokens endpoint). The CPU-bound tiktoken encode itself is deferred to
    // each transport, where it runs on the blocking pool overlapped with the
    // upstream round-trip rather than serially in front of it (see forward_http /
    // forward_websocket); the multi-upstream chain races it against each
    // attempt's dispatch (the pool's first poll via `pooled_first_poll`, the
    // non-pooled send via `send_classified_with_estimate`). See model/responses.rs.
    //
    // A non-streaming turn emits no `message_start` and would otherwise skip the
    // work, but one carrying `stop_sequences` needs the estimate for a second
    // reason: an emulated stop makes the upstream's own `response.completed`
    // usage a no-op, so without a seed the final JSON reports `input_tokens: 0`
    // for a non-empty prompt (issue #605). `final_json` falls back to the
    // estimate exactly when no usage was observed, so seeding it here is enough.
    let estimate_input = if (client_wants_stream || !stop_sequences.is_empty())
        && matches!(
            state
                .config
                .provider(&route.provider)
                .map(|provider| provider.count_tokens)
                .unwrap_or(CountTokens::Estimate),
            CountTokens::Tiktoken
        ) {
        Some(body.json_arc())
    } else {
        None
    };
    let turn = TurnOptions {
        client_wants_stream,
        thinking_enabled,
        tool_search_native,
        stop_sequences,
        response_byte_cap,
    };
    let upstream_body = Arc::new(translate_request_value(
        request_json,
        &route,
        flavor,
        tool_search_native,
        session_id.as_deref(),
    ));
    tracing::debug!(
        provider = %route.provider,
        upstream_model = %route.upstream_model,
        upstream_request = %upstream_body,
        "responses upstream request"
    );
    let auth = state
        .config
        .provider(&route.provider)
        .map(|provider| provider.auth)
        .unwrap_or_default();

    // Codex/ChatGPT account-pool failover (M10), mirroring the Anthropic
    // adapter's claude_oauth branch: pooled credentials are resolved per-account
    // inside forward_chatgpt_oauth rather than once up front, so a single
    // account's expired/rejected token can rotate to the next one instead of
    // failing the whole request.
    let mut single_started_at: Option<std::time::Instant> = None;
    if auth == AuthMode::ChatgptOauth {
        let provider = state
            .config
            .provider(&route.provider)
            .expect("route provider was validated");
        let ws_enabled = state.config.codex_websocket_enabled(&route.provider);
        if turn.client_wants_stream && !ws_enabled {
            // Commit before the account-store scan: the scan queues an
            // unbounded filesystem walk on the blocking pool, and neither a
            // slow filesystem nor a saturated pool may delay the committed
            // response (keepalive pings cover the wait). The empty-scan
            // single-credential fallback runs inside the same stream.
            let accounts_config = provider.accounts.clone();
            let account_scope = provider.account_scope.clone();
            let scan = Box::pin(async move {
                auth::shared::resolve_pool_accounts(
                    "codex",
                    &accounts_config,
                    &account_scope,
                    crate::accounts::StoreFamily::Chatgpt,
                    auth::codex::store::default_accounts_dir(),
                    auth::codex::store::scan_accounts,
                )
                .await
            });
            let credential_state = state.clone();
            let credential_route = route.clone();
            let credential = CredentialSource::Deferred(Box::pin(async move {
                resolve_credential(
                    &credential_state.config,
                    &credential_route,
                    &credential_state.http_client,
                )
                .await
            }));
            return forward_chatgpt_oauth_stream(
                state,
                route,
                PoolForwardStream {
                    session_id,
                    upstream_body,
                    turn,
                    estimate_input,
                    scan,
                    credential,
                },
            )
            .await;
        }
        // The empty-scan single-credential fallback below commits its stream
        // after this scan (and a possible websocket attempt) ran: seed its
        // sample clock here so `shunt.latency` keeps the dispatch span the
        // pre-commit loop records.
        single_started_at = Some(std::time::Instant::now());
        let accounts = auth::shared::resolve_pool_accounts(
            "codex",
            &provider.accounts,
            &provider.account_scope,
            crate::accounts::StoreFamily::Chatgpt,
            auth::codex::store::default_accounts_dir(),
            auth::codex::store::scan_accounts,
        )
        .await
        .map_err(own_error)?;
        if !accounts.is_empty() {
            return forward_chatgpt_oauth(
                state,
                route,
                PoolForward {
                    pool_key,
                    session_id,
                    upstream_body,
                    accounts_config: accounts,
                    turn,
                    estimate_input,
                },
            )
            .await;
        }
        // No [[accounts]] configured and none found in the store: fall through
        // to the single-account path below (backward-compat with
        // `auth = "chatgpt_oauth"` configured without any pooled accounts).
    }

    // The single-credential HTTP path defers resolution into the committed
    // stream (see `CredentialSource`); the websocket path has no early
    // commit, so it resolves up front exactly as before.
    let (credential, codex_quota_account) = if state.config.codex_websocket_enabled(&route.provider)
    {
        let credential = resolve_credential(&state.config, &route, &state.http_client).await?;
        let codex_quota_account = codex_quota_account(&credential);
        (CredentialSource::Resolved(credential), codex_quota_account)
    } else {
        let deferred_state = state.clone();
        let deferred_route = route.clone();
        (
            CredentialSource::Deferred(Box::pin(async move {
                resolve_credential(
                    &deferred_state.config,
                    &deferred_route,
                    &deferred_state.http_client,
                )
                .await
            })),
            None,
        )
    };
    // Codex WebSocket v2 transport (issue #32), opt-in per provider and only for
    // the ChatGPT/Codex backend. HTTP stays the path for every other upstream, and
    // is the documented safety net: any websocket failure before the first event
    // reaches the client — connect, handshake, send, or a socket that drops before
    // the first event (issue #46) — transparently falls back to the HTTP path
    // below, so enabling the flag can never do worse than plain HTTP. Only a
    // failure *after* the first event surfaces mid-stream — an Anthropic `error`
    // event to a streaming client, or a gateway error to a non-streaming one —
    // since by then the response has already begun and cannot be safely restarted.
    if state.config.codex_websocket_enabled(&route.provider) {
        // This branch built `Resolved` above: the websocket path has no
        // early commit and must not defer.
        let CredentialSource::Resolved(websocket_credential) = &credential else {
            return Err(own_error(
                "responses websocket path built a deferred credential".to_string(),
            ));
        };
        let websocket_options = ForwardOptions {
            upstream_body: upstream_body.clone(),
            auth,
            turn: turn.clone(),
            codex_quota_account: codex_quota_account.clone(),
            estimate_input: estimate_input.clone(),
            started_at: None,
        };
        match forward_websocket(
            &state,
            &route,
            pool_key.as_deref(),
            session_id.as_deref(),
            websocket_options,
            websocket_credential.clone(),
        )
        .await
        {
            Ok(response) => return Ok(response),
            Err(error) if error.failure.is_some() => {
                tracing::warn!(
                    provider = %route.provider,
                    error = %error.message,
                    "codex websocket failed before streaming; falling back to HTTP"
                );
            }
            Err(error) => return Err(error),
        }
    }
    let forward_options = ForwardOptions {
        upstream_body,
        auth,
        turn,
        codex_quota_account,
        estimate_input,
        started_at: single_started_at,
    };
    forward_http(
        &state,
        &route,
        forward_options,
        credential,
        session_id.as_deref(),
    )
    .await
}

/// The default unpooled Codex CLI credential is still an observed account:
/// attach its stable account id to quota capture so the read-only admin view
/// can display x-codex-* response-derived usage without importing or copying
/// the credential into shunt's managed account store.
pub(super) fn codex_quota_account(credential: &Credential) -> Option<crate::config::AccountConfig> {
    match credential {
        Credential::ChatGptOAuth { account_id, .. } => Some(crate::config::AccountConfig {
            name: "local-codex".to_string(),
            uuid: Some(account_id.clone()),
            ..Default::default()
        }),
        _ => None,
    }
}

/// One upstream attempt for the multi-upstream streaming chain
/// (`proxy::chain_stream`): the per-route preparation of [`forward`] without
/// the response commit, so the chain can drive the send inside the committed
/// stream and classify the outcome itself. A pooled `chatgpt_oauth` route
/// yields the account-rotating pool stream as its attempt — pool exhaustion
/// surfaces as the pool's own terminal error frame, mirroring the pre-commit
/// path's terminal behavior for that route.
pub(crate) async fn chain_attempt(
    state: &AppState,
    route: &Route,
    headers: &HeaderMap,
    body: RequestBody,
    estimate_cache: &Arc<ChainEstimate>,
) -> crate::proxy::chain_stream::Attempt {
    let session_id = headers
        .get("x-claude-code-session-id")
        .and_then(|value| value.to_str().ok())
        .filter(|session_id| !session_id.is_empty())
        .map(str::to_string);
    let request_json = body.json();
    // The effective conversation id (see `forward`): metadata-only clients
    // still get the upstream affinity headers and the matching body key.
    let session_id = session_id
        .or_else(|| crate::model::responses_request::effective_session_id(request_json, None));
    let thinking_enabled = request_json
        .pointer("/thinking/type")
        .and_then(Value::as_str)
        == Some("enabled");
    let flavor = state.config.responses_flavor(&route.provider);
    let tool_search_native = state
        .config
        .native_tool_search(&route.provider, &route.upstream_model);
    // See `forward`'s matching extraction: the Responses API has no `stop`
    // parameter, so `stop_sequences` is emulated gateway-side rather than
    // forwarded upstream (issue #605).
    let stop_sequences = request_json
        .get("stop_sequences")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .filter(|sequence| !sequence.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let turn = TurnOptions {
        client_wants_stream: true,
        thinking_enabled,
        tool_search_native,
        stop_sequences,
        // This path is streaming by construction, so nothing here buffers a
        // whole reply for the cap to bound.
        response_byte_cap: None,
    };
    let upstream_body = Arc::new(translate_request_value(
        request_json,
        route,
        flavor,
        tool_search_native,
        session_id.as_deref(),
    ));
    let auth = state
        .config
        .provider(&route.provider)
        .map(|provider| provider.auth)
        .unwrap_or_default();

    if auth == AuthMode::ChatgptOauth {
        let provider = state
            .config
            .provider(&route.provider)
            .expect("route provider was validated");
        let accounts = match auth::shared::resolve_pool_accounts(
            "codex",
            &provider.accounts,
            &provider.account_scope,
            crate::accounts::StoreFamily::Chatgpt,
            auth::codex::store::default_accounts_dir(),
            auth::codex::store::scan_accounts,
        )
        .await
        {
            Ok(accounts) => accounts,
            Err(error) => {
                let envelope = adapter_error_envelope(own_error(error)).await;
                return crate::proxy::chain_stream::Attempt::Failed {
                    advance: false,
                    remember: false,
                    envelope: crate::proxy::chain_stream::LazyEnvelope::Ready(envelope),
                    status: StatusCode::BAD_GATEWAY,
                };
            }
        };
        if !accounts.is_empty() {
            if accounts.iter().all(|account| account.disabled) {
                tracing::warn!(
                    provider = %route.provider,
                    accounts = accounts.len(),
                    "all accounts for provider are disabled; none are selectable"
                );
                let envelope = adapter_error_envelope(own_error(format!(
                    "provider '{}' has {} account(s) but all are `disabled = true`; none are selectable",
                    route.provider,
                    accounts.len()
                )))
                .await;
                return crate::proxy::chain_stream::Attempt::Failed {
                    advance: false,
                    remember: false,
                    envelope: crate::proxy::chain_stream::LazyEnvelope::Ready(envelope),
                    status: StatusCode::BAD_GATEWAY,
                };
            }
            let (order, reprobe) = state.accounts.select_order_deferred(
                &route.provider,
                &accounts,
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
                accounts_config: std::sync::Arc::new(accounts),
                order,
                reprobe,
                ramp_initial: state.config.storm_ramp_initial(),
                record_metrics: false,
                started_at: None,
            });
            // Race the machine build — which awaits the bounded token
            // estimate — against the pool's first poll so account admission,
            // credential resolution, and the upstream request overlap the
            // estimate instead of serializing behind it (the pooled-branch
            // counterpart of translated_core's leading race). The pool's
            // first item, when it wins, is buffered, and only the winner arm
            // consumes the build: a pre-frame exhaustion is a classified
            // failure the chain advances on like the pre-commit loop (§4) —
            // never a mid-relay terminal event — and it never waits on the
            // estimate. The chain's cache hands every opted-in attempt the
            // same estimate share: an exhausted attempt drops its share, not
            // the compute, and the next opted-in attempt resumes it instead
            // of re-tokenizing the identical body.
            let build = {
                let state = state.clone();
                let route = route.clone();
                let request = body.json_arc();
                let cache = estimate_cache.clone();
                async move {
                    let estimate_value = if counts_locally(&state, &route) {
                        let state = state.clone();
                        let route = route.clone();
                        let request = request.clone();
                        let shared = cache
                            .get_or_start(move || async move {
                                winner_estimate(&state, &route, &request).await
                            })
                            .await;
                        shared.await
                    } else {
                        0
                    };
                    let mut machine = turn
                        .relay(&route)
                        .machine()
                        .with_input_estimate(estimate_value)
                        .without_content_accumulation();
                    let start = machine.start_synthetic(format!("msg_{}", uuid::Uuid::new_v4()));
                    (machine, start)
                }
            };
            let PoolFirstPoll {
                build,
                item,
                events,
                headers_at,
            } = pooled_first_poll(Box::pin(events), build).await;
            // The synthetic start stays deferred until an account actually
            // wins; the header-latency sample is the first item's arrival
            // (captured by the seam), never inflated by a still-pending
            // estimate.
            match item {
                Some(Ok(PoolItem::Event(event))) => {
                    return crate::proxy::chain_stream::Attempt::Winner {
                        headers_at,
                        relay: pool_relay_build(build, event, events),
                    };
                }
                Some(Ok(PoolItem::Exhausted {
                    status,
                    advance,
                    remember,
                    envelope,
                })) => {
                    // The pending build is dropped with the failure: the
                    // estimate share it holds keeps the chain's single
                    // compute alive for the next opted-in attempt, and no
                    // synthetic start reaches the client on a failed
                    // attempt.
                    return crate::proxy::chain_stream::Attempt::Failed {
                        advance,
                        remember,
                        envelope,
                        status,
                    };
                }
                Some(Err(envelope)) => {
                    // The TTFB timeout (the arbiter: terminal) or a
                    // non-failover relayed status — both terminal, neither
                    // remembered.
                    return crate::proxy::chain_stream::Attempt::Failed {
                        advance: false,
                        remember: false,
                        envelope: crate::proxy::chain_stream::LazyEnvelope::Ready(envelope),
                        status: StatusCode::BAD_GATEWAY,
                    };
                }
                None => {
                    return crate::proxy::chain_stream::Attempt::Failed {
                        advance: true,
                        remember: false,
                        envelope: crate::proxy::chain_stream::LazyEnvelope::Ready(
                            adapter_error_envelope(transport_error(
                                "all Codex OAuth accounts failed before receiving an upstream response"
                                    .to_string(),
                            ))
                            .await,
                        ),
                        status: StatusCode::BAD_GATEWAY,
                    };
                }
            }
        }
        // No accounts: fall through to the single-credential path, exactly
        // like `forward`.
    }

    let credential = match resolve_credential(&state.config, route, &state.http_client).await {
        Ok(credential) => credential,
        Err(error) => {
            // Capture before the error is consumed: the envelope the stream
            // emits is the credential error's own (a missing key is a 401),
            // and the chain must classify the attempt with that status, not
            // a gateway-synthesized 502.
            let status = error.response.status();
            let envelope = adapter_error_envelope(error).await;
            return crate::proxy::chain_stream::Attempt::Failed {
                advance: false,
                remember: false,
                envelope: crate::proxy::chain_stream::LazyEnvelope::Ready(envelope),
                status,
            };
        }
    };
    let codex_quota_account = codex_quota_account(&credential);
    let policy = state
        .config
        .provider(&route.provider)
        .map(|provider| provider.retry.policy())
        .unwrap_or(crate::retry::RetryPolicy::DISABLED);
    let send_context = HttpSendContext {
        state: state.clone(),
        route: route.clone(),
        policy,
        credential: Some(credential),
        session_id,
        upstream_body: upstream_body.clone(),
        auth,
        codex_quota_account,
    };
    // Start the route-gated estimate before the send so the encode overlaps
    // the upstream round-trip instead of serializing after the headers
    // arrive — the non-pooled counterpart of the pooled branch's race.
    // Only the winner's pending relay build awaits it, after the chain
    // recorded the winner; a failed attempt drops its share of the chain's
    // single estimate, never the compute, and the next opted-in attempt
    // reuses it.
    let estimate = {
        let cache = estimate_cache.clone();
        let state = state.clone();
        let route = route.clone();
        let request = body.json_arc();
        async move {
            if counts_locally(&state, &route) {
                let state = state.clone();
                let route = route.clone();
                let request = request.clone();
                let shared = cache
                    .get_or_start(
                        move || async move { winner_estimate(&state, &route, &request).await },
                    )
                    .await;
                shared.await
            } else {
                0
            }
        }
    };
    let sent = send_classified_with_estimate(send_classified(&send_context), estimate).await;
    match sent.classified {
        SendClassified::Relay { bytes } => {
            let route = route.clone();
            let relay = relay_build(
                sent.estimate,
                move |estimate_value| {
                    let mut machine = turn
                        .relay(&route)
                        .machine()
                        .with_input_estimate(estimate_value)
                        .without_content_accumulation();
                    let start = machine.start_synthetic(format!("msg_{}", uuid::Uuid::new_v4()));
                    (machine, start)
                },
                move |machine| {
                    Box::pin(
                        translated_stream(parsed_events(bytes), machine)
                            .map_err(|never| match never {}),
                    )
                },
            );
            crate::proxy::chain_stream::Attempt::Winner {
                headers_at: sent.headers_at,
                relay,
            }
        }
        SendClassified::Failed {
            envelope,
            status,
            remember,
            advance,
        } => crate::proxy::chain_stream::Attempt::Failed {
            advance,
            remember,
            envelope,
            status,
        },
    }
}

/// Whether the route opted into local token counting — the estimate gate at
/// every call site. A provider opted out must not receive a locally computed
/// seed, and an opted-out attempt must never start the chain's cached
/// compute: a later opted-in winner would otherwise inherit the opted-out
/// route's zero.
fn counts_locally(state: &AppState, route: &Route) -> bool {
    state
        .config
        .provider(&route.provider)
        .map(|provider| provider.count_tokens == CountTokens::Tiktoken)
        .unwrap_or(false)
}

/// One chain's shared token estimate: the first opted-in attempt starts the
/// bounded blocking encode and every later opted-in attempt reuses the same
/// compute instead of re-tokenizing the identical request body. A failed
/// attempt drops its racing share, never the compute — the cell keeps one
/// handle alive for the chain, so rapid status/transport failures leave at
/// most one tokenization running, not one per attempt.
#[derive(Default)]
pub(crate) struct ChainEstimate {
    cell: tokio::sync::OnceCell<Shared<BoxFuture<'static, u64>>>,
}

impl ChainEstimate {
    /// Start the chain's single estimate on the first call and hand back a
    /// share of it. The factory runs at most once per chain: a share dropped
    /// mid-compute leaves the compute running and the next share resumes it.
    pub(crate) async fn get_or_start<F, Fut>(&self, factory: F) -> Shared<BoxFuture<'static, u64>>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = u64> + Send + 'static,
    {
        self.cell
            .get_or_init(|| {
                std::future::ready({
                    let boxed: BoxFuture<'static, u64> = Box::pin(factory());
                    boxed.shared()
                })
            })
            .await
            .clone()
    }
}

#[cfg(test)]
impl ChainEstimate {
    /// A chain estimate whose cell already holds a pending share: the
    /// factory never runs and every caller's share stays pending, so a test
    /// pins the send-win race and holds the estimate open past it.
    pub(crate) fn test_held_pending() -> std::sync::Arc<Self> {
        let estimate = std::sync::Arc::new(Self::default());
        let boxed: BoxFuture<'static, u64> = Box::pin(std::future::pending());
        estimate
            .cell
            .set(boxed.shared())
            .map_err(|_| ())
            .expect("the test estimate seeds once");
        estimate
    }
}

/// The synthetic start's input-token estimate, gated on the WINNING route's
/// own `count_tokens` setting: a provider opted out of local counting must
/// not receive a locally computed seed just because another chain route
/// counts. The blocking encode runs here, raced against the attempt's
/// dispatch — the pool's first poll via [`pooled_first_poll`], the
/// non-pooled send via [`send_classified_with_estimate`] — with the same
/// one-second bound as the single-route path; a slow estimate delays the
/// deferred synthetic start, never the committed response. Called at most
/// once per chain from [`ChainEstimate`]: every opted-in attempt shares the
/// same encode and its one-second bound.
async fn winner_estimate(state: &AppState, route: &Route, request: &Arc<Value>) -> u64 {
    if !counts_locally(state, route) {
        return 0;
    }
    let request = request.clone();
    let handle = tokio::task::spawn_blocking(move || {
        crate::count_tokens::count_input_tokens_value(&request)
    });
    tokio::time::timeout(std::time::Duration::from_secs(1), handle)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or(0)
}
