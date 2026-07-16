pub mod codex_continuation;
pub mod codex_ws;

mod context;
mod error;
mod http;
mod inbound;
mod pool;
mod request;
mod websocket;
mod ws_stream;

use std::sync::Arc;

use axum::http::{HeaderMap, StatusCode, Uri};
use serde_json::Value;

use crate::{
    adapters::{Adapter, AdapterError, AdapterFuture},
    auth::{self, resolve_credential},
    config::{AuthMode, CountTokens},
    model::responses::translate_request,
    routing::Route,
    server::AppState,
};

use self::context::{ForwardOptions, PoolForward, TurnOptions};
use self::error::own_error;
use self::http::forward_http;
pub(crate) use self::inbound::forward_codex_inbound;
use self::pool::forward_chatgpt_oauth;
use self::websocket::forward_websocket;

pub struct ResponsesAdapter;

impl Adapter for ResponsesAdapter {
    fn forward<'a>(
        &'a self,
        state: AppState,
        route: Route,
        _uri: &'a Uri,
        headers: &'a HeaderMap,
        body: Vec<u8>,
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
    body: Vec<u8>,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let request_json = serde_json::from_slice::<Value>(&body).ok();
    let client_wants_stream = request_json
        .as_ref()
        .and_then(|value| value.get("stream").and_then(Value::as_bool))
        .unwrap_or(false);
    // Gates reasoning round-tripping (see model/responses.rs): surface thinking
    // blocks only when the client asked for extended thinking, since that is what
    // makes Claude Code echo them back on the next turn.
    let thinking_enabled = request_json
        .as_ref()
        .and_then(|value| value.pointer("/thinking/type").and_then(Value::as_str))
        == Some("enabled");
    let flavor = state.config.responses_flavor(&route.provider);
    // Native client-executed tool_search (issue #82) is opt-in per provider and
    // gated on flavor + model; otherwise the #43 progressive-reveal shim is used.
    let tool_search_native = state
        .config
        .native_tool_search(&route.provider, &route.upstream_model);
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
        request_json.map(Arc::new)
    } else {
        None
    };
    let turn = TurnOptions {
        client_wants_stream,
        thinking_enabled,
        tool_search_native,
    };
    let upstream_body = translate_request(&body, &route, flavor, tool_search_native)
        .map_err(|error| own_error(error.to_string()))?;
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
                },
            )
            .await;
        }
        // No [[accounts]] configured and none found in the store: fall through
        // to the single-account path below (backward-compat with
        // `auth = "chatgpt_oauth"` configured without any pooled accounts).
    }

    let credential = resolve_credential(&state.config, &route, &state.http_client).await?;
    let forward_options = ForwardOptions {
        upstream_body,
        credential,
        auth,
        turn,
        codex_quota_account: None,
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
            Err(error) => {
                tracing::warn!(
                    provider = %route.provider,
                    error = %error.message,
                    "codex websocket failed before streaming; falling back to HTTP"
                );
            }
        }
    }
    forward_http(&state, &route, forward_options, session_id.as_deref()).await
}
