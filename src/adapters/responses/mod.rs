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

use self::context::{ForwardOptions, PoolForward, TurnOptions};
use self::early_stream::{
    parsed_events, pool_translated_stream, send_classified, translated_stream, HttpSendContext,
    SendClassified,
};
use self::error::{adapter_error_envelope, own_error};
use self::http::forward_http;
pub(crate) use self::inbound::forward_codex_inbound;
pub(crate) use self::inbound_routed::forward_codex_routed;
use self::pool::{forward_chatgpt_oauth, pool_events_stream, PoolStreamContext};
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
            forward(state, route, pool_key, session_id.map(str::to_string), body).await
        })
    }
}

async fn forward(
    state: AppState,
    route: Route,
    pool_key: Option<String>,
    session_id: Option<String>,
    body: RequestBody,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let request_json = body.json();
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
    // Seed message_start's usage.input_tokens with a local tiktoken estimate of
    // the (already-parsed) request so Claude Code's per-subagent progress
    // tracker — which reads that first snapshot and never re-reads the merged
    // total — shows a live context figure for codex subagents instead of a stuck
    // 0. The Responses API only reports real usage at response.completed, by
    // which point message_start is long sent; the accurate total still lands in
    // the terminal message_delta. Only streaming turns emit message_start, so
    // non-streaming requests carry `None` and skip the work; gated on the
    // provider's local-counting opt-in (the same CountTokens knob as the
    // count_tokens endpoint). The CPU-bound tiktoken encode itself is deferred to
    // each transport, where it runs on the blocking pool overlapped with the
    // upstream round-trip rather than serially in front of it (see forward_http /
    // forward_websocket). See model/responses.rs.
    let estimate_input = if client_wants_stream
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
    };
    let upstream_body = Arc::new(translate_request_value(
        request_json,
        &route,
        flavor,
        tool_search_native,
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
    if auth == AuthMode::ChatgptOauth {
        let provider = state
            .config
            .provider(&route.provider)
            .expect("route provider was validated");
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

    let credential = resolve_credential(&state.config, &route, &state.http_client).await?;
    // The default unpooled Codex CLI credential is still an observed account:
    // attach its stable account id to quota capture so the read-only admin view
    // can display x-codex-* response-derived usage without importing or copying
    // the credential into shunt's managed account store.
    let codex_quota_account = match &credential {
        Credential::ChatGptOAuth { account_id, .. } => Some(crate::config::AccountConfig {
            name: "local-codex".to_string(),
            uuid: Some(account_id.clone()),
            ..Default::default()
        }),
        _ => None,
    };
    let forward_options = ForwardOptions {
        upstream_body,
        credential,
        auth,
        turn,
        codex_quota_account,
        estimate_input,
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
        match forward_websocket(&state, &route, pool_key.as_deref(), forward_options.clone()).await
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
    forward_http(&state, &route, forward_options, session_id.as_deref()).await
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
    estimate: u64,
) -> crate::proxy::chain_stream::Attempt {
    let session_id = headers
        .get("x-claude-code-session-id")
        .and_then(|value| value.to_str().ok())
        .filter(|session_id| !session_id.is_empty())
        .map(str::to_string);
    let request_json = body.json();
    let thinking_enabled = request_json
        .pointer("/thinking/type")
        .and_then(Value::as_str)
        == Some("enabled");
    let flavor = state.config.responses_flavor(&route.provider);
    let tool_search_native = state
        .config
        .native_tool_search(&route.provider, &route.upstream_model);
    let turn = TurnOptions {
        client_wants_stream: true,
        thinking_enabled,
        tool_search_native,
    };
    let upstream_body = Arc::new(translate_request_value(
        request_json,
        route,
        flavor,
        tool_search_native,
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
                    envelope,
                    status: StatusCode::BAD_GATEWAY,
                };
            }
        };
        if !accounts.is_empty() {
            let mut machine = turn
                .relay(route)
                .machine()
                .with_input_estimate(estimate)
                .without_content_accumulation();
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
            });
            let start = machine.start_synthetic(format!("msg_{}", uuid::Uuid::new_v4()));
            return crate::proxy::chain_stream::Attempt::Winner {
                start: Some(axum::body::Bytes::from(start.join(""))),
                frames: Box::pin(pool_translated_stream(events, machine)),
            };
        }
        // No accounts: fall through to the single-credential path, exactly
        // like `forward`.
    }

    let credential = match resolve_credential(&state.config, route, &state.http_client).await {
        Ok(credential) => credential,
        Err(error) => {
            let envelope = adapter_error_envelope(error).await;
            return crate::proxy::chain_stream::Attempt::Failed {
                advance: false,
                envelope,
                status: StatusCode::BAD_GATEWAY,
            };
        }
    };
    let codex_quota_account = match &credential {
        Credential::ChatGptOAuth { account_id, .. } => Some(crate::config::AccountConfig {
            name: "local-codex".to_string(),
            uuid: Some(account_id.clone()),
            ..Default::default()
        }),
        _ => None,
    };
    let policy = state
        .config
        .provider(&route.provider)
        .map(|provider| provider.retry.policy())
        .unwrap_or(crate::retry::RetryPolicy::DISABLED);
    let prepared = self::body::prepare_body(state, route, upstream_body.as_ref()).await;
    let send_context = HttpSendContext {
        state: state.clone(),
        route: route.clone(),
        policy,
        credential,
        session_id,
        body: prepared,
        auth,
        codex_quota_account,
    };
    match send_classified(&send_context).await {
        SendClassified::Relay { bytes } => {
            let mut machine = turn
                .relay(route)
                .machine()
                .with_input_estimate(estimate)
                .without_content_accumulation();
            let start = machine.start_synthetic(format!("msg_{}", uuid::Uuid::new_v4()));
            crate::proxy::chain_stream::Attempt::Winner {
                start: Some(axum::body::Bytes::from(start.join(""))),
                frames: Box::pin(translated_stream(parsed_events(bytes), machine)),
            }
        }
        SendClassified::Failed { envelope, status } => {
            crate::proxy::chain_stream::Attempt::Failed {
                advance: crate::proxy::failover::is_advance_status(status),
                envelope,
                status,
            }
        }
    }
}
