pub mod codex_continuation;
pub mod codex_ws;

use std::{path::PathBuf, sync::Arc, time::Duration};

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderValue, Response, StatusCode, Uri},
    response::IntoResponse,
};
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};

use self::codex_ws::{CodexWsError, CodexWsEvents};
use crate::{
    accounts::{self, FailoverAction},
    adapters::{Adapter, AdapterError, AdapterFuture},
    auth::{
        self, codex::auth::CodexAuthStore, resolve_chatgpt_account, resolve_credential, Credential,
    },
    config::{AccountConfig, AuthMode, CountTokens},
    error::ShuntError,
    model::responses::{
        map_error_value, parse_sse_events, translate_request, AnthropicSseMachine, ResponseEvent,
    },
    routing::Route,
    server::AppState,
};

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
        let accounts = if provider.accounts.is_empty() {
            // scan_accounts() does synchronous directory + file I/O; run it on
            // the blocking pool so it never stalls a runtime worker thread.
            tokio::task::spawn_blocking(auth::codex::store::scan_accounts)
                .await
                .map_err(|error| {
                    own_error(format!("codex account store scan task failed: {error}"))
                })?
                .map_err(|error| {
                    own_error(format!(
                        "failed to scan codex account store {}: {error}",
                        auth::codex::store::default_accounts_dir().display()
                    ))
                })?
        } else {
            provider.accounts.clone()
        };
        if !accounts.is_empty() {
            return forward_chatgpt_oauth(
                state,
                route,
                pool_key,
                session_id,
                upstream_body,
                accounts,
                client_wants_stream,
                thinking_enabled,
                tool_search_native,
            )
            .await;
        }
        // No [[accounts]] configured and none found in the store: fall through
        // to the single-account path below (backward-compat with
        // `auth = "chatgpt_oauth"` configured without any pooled accounts).
    }

    let credential = resolve_credential(&state.config, &route, &state.http_client).await?;
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
        match forward_websocket(
            &state,
            &route,
            pool_key.as_deref(),
            upstream_body.clone(),
            credential.clone(),
            auth,
            client_wants_stream,
            thinking_enabled,
            tool_search_native,
            estimate_input.clone(),
        )
        .await
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
    forward_http(
        &state,
        &route,
        upstream_body,
        credential,
        auth,
        client_wants_stream,
        thinking_enabled,
        tool_search_native,
        estimate_input,
        session_id.as_deref(),
    )
    .await
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
/// `note_quota` is never called here — unlike Anthropic, the ChatGPT backend
/// carries no per-account quota-rejection headers, so failover in this
/// adapter is cooldown-based only.
#[allow(clippy::too_many_arguments)]
async fn forward_chatgpt_oauth(
    state: AppState,
    route: Route,
    pool_key: Option<String>,
    session_id: Option<String>,
    upstream_body: Value,
    accounts_config: Vec<AccountConfig>,
    client_wants_stream: bool,
    thinking_enabled: bool,
    tool_search_native: bool,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let order = state.accounts.select_order(
        &route.provider,
        &accounts_config,
        session_id.as_deref(),
        // Codex carries no per-model quota signal to order by, unlike the
        // Anthropic pool's rate-limit headers.
        None,
    );
    let ws_enabled = state.config.codex_websocket_enabled(&route.provider);
    let auth = AuthMode::ChatgptOauth;
    let mut last_response: Option<reqwest::Response> = None;

    for index in order {
        let account = &accounts_config[index];
        // The per-account refresh_lock serializes only credential refreshes for
        // one account (see the matching note in
        // anthropic::forward_claude_oauth) — never held across an upstream send.
        let refresh_lock = state.accounts.refresh_lock(&route.provider, &account.name);

        let credential = {
            let _guard = refresh_lock.lock().await;
            match resolve_chatgpt_account(account, &state.http_client).await {
                Ok(credential) => credential,
                Err(error) => {
                    state.accounts.cooldown(
                        &route.provider,
                        &account.name,
                        Duration::from_secs(5 * 60),
                    );
                    tracing::warn!(
                        provider = %route.provider,
                        account = %account.name,
                        error = %error.message,
                        "failed to resolve ChatGPT OAuth account"
                    );
                    continue;
                }
            }
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
                upstream_body.clone(),
                credential.clone(),
                auth,
                client_wants_stream,
                thinking_enabled,
                tool_search_native,
                // Pool path does not pre-compute a message_start input estimate
                // yet (see relay_success) — follow-up to thread it through here.
                None,
            )
            .await
            {
                Ok((status, response)) => {
                    state.accounts.mark_healthy(&route.provider, &account.name);
                    return Ok((status, with_account_header(response, &account.name)));
                }
                Err(error) => {
                    // A pre-stream websocket failure (connect/handshake/send) falls
                    // back to HTTP on the SAME account, exactly like the
                    // single-account path in `forward` — only an HTTP failure
                    // triggers account-pool failover below. (A mid-stream failure
                    // is instead surfaced as an SSE error event and never reaches
                    // here — the response has already begun by then.)
                    tracing::warn!(
                        provider = %route.provider,
                        account = %account.name,
                        error = %error.message,
                        "codex websocket failed before streaming; falling back to HTTP for this account"
                    );
                }
            }
        }

        let upstream = match http_send(
            &state,
            &route,
            credential.clone(),
            session_id.as_deref(),
            &upstream_body,
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                state
                    .accounts
                    .cooldown(&route.provider, &account.name, Duration::from_secs(30));
                tracing::warn!(
                    provider = %route.provider,
                    account = %account.name,
                    error = %error.message,
                    "ChatGPT OAuth upstream request failed"
                );
                continue;
            }
        };

        let status = upstream.status();
        match accounts::classify_codex(status, upstream.headers()) {
            FailoverAction::Relay => {
                // A non-401/429/5xx response means the account itself is fine,
                // whether or not this particular request succeeded (mirrors
                // the Anthropic adapter's top-level Relay arm).
                state.accounts.mark_healthy(&route.provider, &account.name);
                if status.is_success() {
                    let response = relay_success(
                        &state,
                        &route,
                        upstream,
                        client_wants_stream,
                        thinking_enabled,
                        tool_search_native,
                    )
                    .await?;
                    return Ok((StatusCode::OK, with_account_header(response, &account.name)));
                }
                // A non-failover 4xx (e.g. 400) is a client error, not the
                // account's fault: relay it (re-shaped into the Anthropic error
                // envelope by mapped_upstream_error, as everywhere on this path)
                // rather than rotating to another account.
                return Err(mapped_upstream_error(status, upstream, auth).await);
            }
            FailoverAction::Rotate => {
                let cooldown = if status == StatusCode::TOO_MANY_REQUESTS {
                    accounts::retry_after(upstream.headers())
                        .unwrap_or(Duration::from_secs(60))
                        .clamp(Duration::from_secs(1), Duration::from_secs(3600))
                } else {
                    Duration::from_secs(30)
                };
                state
                    .accounts
                    .cooldown(&route.provider, &account.name, cooldown);
                tracing::warn!(
                    provider = %route.provider,
                    account = %account.name,
                    status = %status,
                    "ChatGPT OAuth account failed over; cooling down and rotating to the next account"
                );
                last_response = Some(upstream);
            }
            FailoverAction::RefreshRetry => {
                // Unlike Claude, Codex's store never encodes a non-refreshable
                // "long-lived setup token" shape (see auth/codex/store.rs) — the
                // only static credential source is an explicit `token_env`.
                if account.token_env.is_some() {
                    state.accounts.cooldown(
                        &route.provider,
                        &account.name,
                        Duration::from_secs(5 * 60),
                    );
                    tracing::warn!(
                        provider = %route.provider,
                        account = %account.name,
                        "ChatGPT OAuth account returned 401 but its credential is not refreshable (token_env); cooling down"
                    );
                    last_response = Some(upstream);
                    continue;
                }

                let credentials_path = account
                    .credentials
                    .as_deref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| auth::codex::store::account_path(&account.name));
                let store = CodexAuthStore::new(credentials_path, state.http_client.clone());
                // Serialize the refresh + credential writeback for this account
                // (see the refresh_lock note at the top of the loop); release
                // the lock again before the retry send below.
                let refreshed = {
                    let _guard = refresh_lock.lock().await;
                    match store.force_refresh().await {
                        Ok(credential) => credential,
                        Err(error) => {
                            state.accounts.cooldown(
                                &route.provider,
                                &account.name,
                                Duration::from_secs(5 * 60),
                            );
                            tracing::warn!(
                                provider = %route.provider,
                                account = %account.name,
                                error = %error.message,
                                "failed to force-refresh ChatGPT OAuth account"
                            );
                            last_response = Some(upstream);
                            continue;
                        }
                    }
                };
                let retry_credential = Credential::ChatGptOAuth {
                    access_token: refreshed.access_token,
                    account_id: refreshed.account_id,
                };
                let retry = match http_send(
                    &state,
                    &route,
                    retry_credential,
                    session_id.as_deref(),
                    &upstream_body,
                )
                .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        state.accounts.cooldown(
                            &route.provider,
                            &account.name,
                            Duration::from_secs(30),
                        );
                        tracing::warn!(
                            provider = %route.provider,
                            account = %account.name,
                            error = %error.message,
                            "ChatGPT OAuth refresh retry failed"
                        );
                        last_response = Some(upstream);
                        continue;
                    }
                };
                let retry_status = retry.status();
                if retry_status == StatusCode::UNAUTHORIZED {
                    // Refresh succeeded but the credential is still rejected —
                    // the account is genuinely broken. Cool it down longer and
                    // rotate rather than relaying the 401 to the client.
                    state.accounts.cooldown(
                        &route.provider,
                        &account.name,
                        Duration::from_secs(5 * 60),
                    );
                    tracing::warn!(
                        provider = %route.provider,
                        account = %account.name,
                        "ChatGPT OAuth account refreshed successfully but upstream still rejected the new credential; cooling down and rotating"
                    );
                    last_response = Some(retry);
                    continue;
                }
                // Classify the refreshed retry the same way the initial
                // response is classified, so a non-success outcome fails over
                // to the remaining accounts instead of short-circuiting the
                // pool.
                match accounts::classify_codex(retry_status, retry.headers()) {
                    FailoverAction::Relay => {
                        if retry_status.is_success() {
                            state.accounts.mark_healthy(&route.provider, &account.name);
                            let response = relay_success(
                                &state,
                                &route,
                                retry,
                                client_wants_stream,
                                thinking_enabled,
                                tool_search_native,
                            )
                            .await?;
                            return Ok((
                                StatusCode::OK,
                                with_account_header(response, &account.name),
                            ));
                        }
                        return Err(mapped_upstream_error(retry_status, retry, auth).await);
                    }
                    // Exhaustive rather than `_` so a new FailoverAction variant
                    // forces a decision here. Two of these arms are unreachable
                    // for the retry status and are matched only to document the
                    // invariants at the call site: classify_codex returns
                    // RefreshRetry only for 401, but a 401 retry already `continue`d
                    // at the `retry_status == UNAUTHORIZED` check above, so it never
                    // reaches this match; and it never returns PauseSame at all (its
                    // 429 arm always maps to Rotate). Only Relay and Rotate are live
                    // here — RefreshRetry rides Rotate's arm as a defensive no-op.
                    FailoverAction::Rotate | FailoverAction::RefreshRetry => {
                        let cooldown = if retry_status == StatusCode::TOO_MANY_REQUESTS {
                            accounts::retry_after(retry.headers())
                                .unwrap_or(Duration::from_secs(60))
                                .clamp(Duration::from_secs(1), Duration::from_secs(3600))
                        } else {
                            Duration::from_secs(30)
                        };
                        state
                            .accounts
                            .cooldown(&route.provider, &account.name, cooldown);
                        tracing::warn!(
                            provider = %route.provider,
                            account = %account.name,
                            status = %retry_status,
                            "ChatGPT OAuth refresh retry did not succeed; rotating to the next account"
                        );
                        last_response = Some(retry);
                        continue;
                    }
                    FailoverAction::PauseSame => {
                        unreachable!("classify_codex never returns PauseSame")
                    }
                }
            }
            FailoverAction::PauseSame => unreachable!("classify_codex never returns PauseSame"),
        }
    }

    match last_response {
        Some(upstream) => {
            let status = upstream.status();
            Err(mapped_upstream_error(status, upstream, auth).await)
        }
        None => Err(own_error(
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
    route: &Route,
    upstream: reqwest::Response,
    client_wants_stream: bool,
    thinking_enabled: bool,
    tool_search_native: bool,
) -> Result<axum::response::Response, AdapterError> {
    if client_wants_stream {
        let keepalive = Duration::from_secs(state.config.server.sse_keepalive_seconds);
        // The pool path does not (yet) pre-compute a tiktoken input estimate to
        // seed message_start (#112 threads it only through the single-account
        // forward_http / forward_websocket paths), so pass 0 = "no estimate"
        // here — identical to those paths when the provider opts out of local
        // counting. Extending the estimate to pooled codex turns is a follow-up.
        Ok(stream_response(
            upstream,
            route.model.clone(),
            thinking_enabled,
            tool_search_native,
            0,
            keepalive,
        ))
    } else {
        json_response(
            upstream,
            route.model.clone(),
            thinking_enabled,
            tool_search_native,
        )
        .await
    }
}

/// Inject `x-shunt-account` naming which pool account produced the response,
/// mirroring the Anthropic adapter's `relay_response`. Silently skipped if the
/// account name is not a valid header value — should never happen, since
/// account names are validated against `[a-z0-9-]+` at import time (see
/// `auth::codex::store::validate_account_name`).
fn with_account_header(
    mut response: axum::response::Response,
    account_name: &str,
) -> axum::response::Response {
    if let Ok(value) = HeaderValue::from_str(account_name) {
        response.headers_mut().insert("x-shunt-account", value);
    }
    response
}

/// Entry point for the inbound `[server.codex_endpoint]` passthrough. Gathers the
/// target provider's pooled accounts (explicit `[[accounts]]` or a store scan)
/// and drives the passthrough over the pool; falls back to the single default
/// `~/.codex/auth.json` credential when no pooled accounts exist — mirroring the
/// outbound [`forward`] `chatgpt_oauth` branch so a single-account user keeps
/// working without configuring a pool.
pub(crate) async fn forward_codex_inbound(
    state: AppState,
    route: Route,
    session_id: Option<String>,
    client_headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    // codex -> shunt -> codex is a byte-faithful passthrough: forward the Codex
    // CLI's own request headers verbatim and swap in only the pool account's
    // credential (below). Strip just the shunt client-token header here so it
    // never leaks upstream; credential + framing headers are handled in
    // passthrough_request_headers / passthrough_send.
    let token_header = state
        .inbound_auth
        .as_ref()
        .map(|auth| auth.header().to_string());
    let passthrough_headers = passthrough_request_headers(&client_headers, token_header.as_deref());

    let provider = state
        .config
        .provider(&route.provider)
        .ok_or_else(|| own_error(format!("unknown provider {}", route.provider)))?;
    let accounts = if provider.accounts.is_empty() {
        // scan_accounts() does synchronous directory + file I/O; run it on the
        // blocking pool so it never stalls a runtime worker thread.
        tokio::task::spawn_blocking(auth::codex::store::scan_accounts)
            .await
            .map_err(|error| own_error(format!("codex account store scan task failed: {error}")))?
            .map_err(|error| {
                own_error(format!(
                    "failed to scan codex account store {}: {error}",
                    auth::codex::store::default_accounts_dir().display()
                ))
            })?
    } else {
        provider.accounts.clone()
    };
    if accounts.is_empty() {
        return forward_codex_passthrough_single(state, route, passthrough_headers, body).await;
    }
    forward_codex_passthrough(
        state,
        route,
        accounts,
        session_id,
        passthrough_headers,
        body,
    )
    .await
}

/// Single-account inbound passthrough: no pool, no failover. Resolves the default
/// `chatgpt_oauth` credential (`~/.codex/auth.json` / `$CODEX_AUTH_FILE`), sends
/// the inbound body once, and relays the upstream response verbatim — the
/// backward-compatible path when no `[[accounts]]` are configured or found.
async fn forward_codex_passthrough_single(
    state: AppState,
    route: Route,
    passthrough_headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let credential = resolve_credential(&state.config, &route, &state.http_client).await?;
    let upstream =
        passthrough_send(&state, &route, credential, &passthrough_headers, &body).await?;
    let status = upstream.status();
    Ok((status, relay_passthrough(upstream)))
}

/// Drive a **raw OpenAI Responses passthrough** turn over the Codex/ChatGPT
/// account pool for the inbound `[server.codex_endpoint]` routes. Unlike
/// [`forward_chatgpt_oauth`], the inbound body is sent upstream **verbatim** (no
/// `translate_request`) and the upstream response is relayed **verbatim** (no
/// `AnthropicSseMachine`), so a Codex CLI pointed at shunt talks its own protocol
/// end to end. Only the account-pool machinery is shared: session-sticky
/// selection, per-account refresh, and `classify_codex` failover (429/5xx rotate,
/// 401 force-refresh + retry, cooldowns). On exhaustion the last upstream
/// response is relayed verbatim rather than re-shaped into an Anthropic error.
async fn forward_codex_passthrough(
    state: AppState,
    route: Route,
    accounts_config: Vec<AccountConfig>,
    session_id: Option<String>,
    passthrough_headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let order = state.accounts.select_order(
        &route.provider,
        &accounts_config,
        session_id.as_deref(),
        // Codex exposes no per-model quota signal to order by (same as the
        // Anthropic-translating pool path).
        None,
    );
    let mut last_response: Option<reqwest::Response> = None;

    for index in order {
        let account = &accounts_config[index];
        // The per-account refresh_lock serializes only credential refreshes for
        // one account — never held across an upstream send (same discipline as
        // forward_chatgpt_oauth).
        let refresh_lock = state.accounts.refresh_lock(&route.provider, &account.name);

        let credential = {
            let _guard = refresh_lock.lock().await;
            match resolve_chatgpt_account(account, &state.http_client).await {
                Ok(credential) => credential,
                Err(error) => {
                    state.accounts.cooldown(
                        &route.provider,
                        &account.name,
                        Duration::from_secs(5 * 60),
                    );
                    tracing::warn!(
                        provider = %route.provider,
                        account = %account.name,
                        error = %error.message,
                        "failed to resolve ChatGPT OAuth account for inbound passthrough"
                    );
                    continue;
                }
            }
        };

        let upstream = match passthrough_send(
            &state,
            &route,
            credential.clone(),
            &passthrough_headers,
            &body,
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                state
                    .accounts
                    .cooldown(&route.provider, &account.name, Duration::from_secs(30));
                tracing::warn!(
                    provider = %route.provider,
                    account = %account.name,
                    error = %error.message,
                    "inbound passthrough upstream request failed"
                );
                continue;
            }
        };

        let status = upstream.status();
        match accounts::classify_codex(status, upstream.headers()) {
            // Success or a non-failover 4xx (e.g. 400): the account is fine, so
            // relay the upstream response verbatim — a passthrough client expects
            // the raw Responses body, error or not — and never rotate.
            FailoverAction::Relay => {
                state.accounts.mark_healthy(&route.provider, &account.name);
                return Ok((
                    status,
                    with_account_header(relay_passthrough(upstream), &account.name),
                ));
            }
            FailoverAction::Rotate => {
                let cooldown = if status == StatusCode::TOO_MANY_REQUESTS {
                    accounts::retry_after(upstream.headers())
                        .unwrap_or(Duration::from_secs(60))
                        .clamp(Duration::from_secs(1), Duration::from_secs(3600))
                } else {
                    Duration::from_secs(30)
                };
                state
                    .accounts
                    .cooldown(&route.provider, &account.name, cooldown);
                tracing::warn!(
                    provider = %route.provider,
                    account = %account.name,
                    status = %status,
                    "inbound passthrough account failed over; cooling down and rotating"
                );
                last_response = Some(upstream);
            }
            FailoverAction::RefreshRetry => {
                // A `token_env` account cannot be refreshed (used verbatim).
                if account.token_env.is_some() {
                    state.accounts.cooldown(
                        &route.provider,
                        &account.name,
                        Duration::from_secs(5 * 60),
                    );
                    tracing::warn!(
                        provider = %route.provider,
                        account = %account.name,
                        "inbound passthrough account returned 401 but its credential is not refreshable (token_env); cooling down"
                    );
                    last_response = Some(upstream);
                    continue;
                }

                let credentials_path = account
                    .credentials
                    .as_deref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| auth::codex::store::account_path(&account.name));
                let store = CodexAuthStore::new(credentials_path, state.http_client.clone());
                let refreshed = {
                    let _guard = refresh_lock.lock().await;
                    match store.force_refresh().await {
                        Ok(credential) => credential,
                        Err(error) => {
                            state.accounts.cooldown(
                                &route.provider,
                                &account.name,
                                Duration::from_secs(5 * 60),
                            );
                            tracing::warn!(
                                provider = %route.provider,
                                account = %account.name,
                                error = %error.message,
                                "failed to force-refresh ChatGPT OAuth account for inbound passthrough"
                            );
                            last_response = Some(upstream);
                            continue;
                        }
                    }
                };
                let retry_credential = Credential::ChatGptOAuth {
                    access_token: refreshed.access_token,
                    account_id: refreshed.account_id,
                };
                let retry = match passthrough_send(
                    &state,
                    &route,
                    retry_credential,
                    &passthrough_headers,
                    &body,
                )
                .await
                {
                    Ok(response) => response,
                    Err(error) => {
                        state.accounts.cooldown(
                            &route.provider,
                            &account.name,
                            Duration::from_secs(30),
                        );
                        tracing::warn!(
                            provider = %route.provider,
                            account = %account.name,
                            error = %error.message,
                            "inbound passthrough refresh retry failed"
                        );
                        last_response = Some(upstream);
                        continue;
                    }
                };
                let retry_status = retry.status();
                if retry_status == StatusCode::UNAUTHORIZED {
                    // Refresh succeeded but the credential is still rejected — the
                    // account is genuinely broken. Cool it down longer and rotate.
                    state.accounts.cooldown(
                        &route.provider,
                        &account.name,
                        Duration::from_secs(5 * 60),
                    );
                    tracing::warn!(
                        provider = %route.provider,
                        account = %account.name,
                        "inbound passthrough account refreshed but upstream still rejected the new credential; rotating"
                    );
                    last_response = Some(retry);
                    continue;
                }
                match accounts::classify_codex(retry_status, retry.headers()) {
                    FailoverAction::Relay => {
                        state.accounts.mark_healthy(&route.provider, &account.name);
                        return Ok((
                            retry_status,
                            with_account_header(relay_passthrough(retry), &account.name),
                        ));
                    }
                    // classify_codex returns RefreshRetry only for 401 (handled
                    // above) and never PauseSame; only Rotate is live here.
                    FailoverAction::Rotate | FailoverAction::RefreshRetry => {
                        let cooldown = if retry_status == StatusCode::TOO_MANY_REQUESTS {
                            accounts::retry_after(retry.headers())
                                .unwrap_or(Duration::from_secs(60))
                                .clamp(Duration::from_secs(1), Duration::from_secs(3600))
                        } else {
                            Duration::from_secs(30)
                        };
                        state
                            .accounts
                            .cooldown(&route.provider, &account.name, cooldown);
                        tracing::warn!(
                            provider = %route.provider,
                            account = %account.name,
                            status = %retry_status,
                            "inbound passthrough refresh retry did not succeed; rotating"
                        );
                        last_response = Some(retry);
                        continue;
                    }
                    FailoverAction::PauseSame => {
                        unreachable!("classify_codex never returns PauseSame")
                    }
                }
            }
            FailoverAction::PauseSame => unreachable!("classify_codex never returns PauseSame"),
        }
    }

    match last_response {
        // Passthrough: relay the last upstream response verbatim (status + body),
        // unlike the Anthropic path which re-shapes it into an error envelope.
        Some(upstream) => {
            let status = upstream.status();
            Ok((status, relay_passthrough(upstream)))
        }
        None => Err(own_error(
            "all Codex OAuth accounts failed before receiving an upstream response".to_string(),
        )),
    }
}

/// Inbound request headers never forwarded upstream on the Codex passthrough:
/// credential headers (re-injected per pool account in [`passthrough_send`]),
/// the internal `x-shunt-inbound-client` label (a client must never spoof it —
/// matches `proxy::check_inbound_auth`), framing headers the HTTP client
/// recomputes, and `accept-encoding` (dropped so the upstream returns an
/// uncompressed body that [`relay_passthrough`] streams through unchanged). Names
/// compare lowercase — `http` normalizes them.
const PASSTHROUGH_STRIP_REQUEST_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "authorization",
    "chatgpt-account-id",
    "accept-encoding",
    // shunt-internal client-identity label — never trust a client-supplied value
    // (the main proxy path strips it in `check_inbound_auth` before re-inserting
    // the authenticated client name).
    "x-shunt-inbound-client",
    // hop-by-hop (RFC 7230 §6.1)
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Upstream response headers dropped when relaying a Codex passthrough response:
/// framing/hop-by-hop headers axum recomputes for the streamed body, `content-encoding`
/// (the request strips `accept-encoding`, so the body arrives uncompressed and is
/// streamed as-is), and `set-cookie`/`set-cookie2` — upstream/edge session cookies
/// (e.g. Cloudflare `__cf_bm` / `cf_clearance`) are bound to shunt's server-side
/// egress, so relaying them would leak that state to an untrusted inbound client.
/// Every other header — `x-codex-turn-state`, request ids, `openai-*`,
/// `retry-after`, `content-type` — is forwarded verbatim.
const PASSTHROUGH_STRIP_RESPONSE_HEADERS: &[&str] = &[
    "content-length",
    "content-encoding",
    "transfer-encoding",
    // Never relay upstream/edge session cookies to the inbound client — they are
    // tied to shunt's egress, not the client (CWE-200 / CWE-201).
    "set-cookie",
    "set-cookie2",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "upgrade",
];

/// Build the upstream header set for a raw codex -> shunt -> codex passthrough:
/// forward every inbound header the Codex CLI sent EXCEPT the ones shunt must own
/// or strip. The credential headers (`authorization`, `chatgpt-account-id`) are
/// re-injected per selected pool account in [`passthrough_send`]; the shunt
/// client-token header (`token_header`) must never leak upstream; framing/hop-by-hop
/// headers are recomputed by the HTTP client. Everything else — `originator`,
/// `version`, `user-agent`, `OpenAI-Beta`, `session-id`, `thread-id`, `x-codex-*`,
/// `content-type`, `accept` — passes through verbatim, so the Codex CLI's real
/// client identity (its actual version, not a shunt-synthesized one) reaches the
/// backend and model version gating behaves exactly as it would against ChatGPT.
fn passthrough_request_headers(client: &HeaderMap, token_header: Option<&str>) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(client.len());
    for (name, value) in client.iter() {
        let name_str = name.as_str();
        if PASSTHROUGH_STRIP_REQUEST_HEADERS.contains(&name_str)
            || token_header.is_some_and(|header| header.eq_ignore_ascii_case(name_str))
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Send the inbound Responses bytes upstream **verbatim** over the Codex HTTP
/// path. Unlike the translating path's [`request_builder`], this forwards the
/// Codex CLI's own request headers (`passthrough_headers`, built by
/// [`passthrough_request_headers`]) and swaps in **only** the selected pool
/// account's credential — no shunt-synthesized client identity — so
/// codex -> shunt -> codex is byte-faithful end to end.
async fn passthrough_send(
    state: &AppState,
    route: &Route,
    credential: Credential,
    passthrough_headers: &HeaderMap,
    body: &Bytes,
) -> Result<reqwest::Response, AdapterError> {
    let mut request = state
        .http_client
        .post(responses_url(&state.config, &route.provider))
        .headers(passthrough_headers.clone());
    match credential {
        Credential::ChatGptOAuth {
            access_token,
            account_id,
        } => {
            request = request
                .bearer_auth(access_token)
                .header("chatgpt-account-id", account_id);
        }
        // A codex_endpoint provider is validated to be chatgpt_oauth, so only the
        // arm above runs in practice; the rest keep the credential swap defensive
        // without ever adding a synthetic client-identity header.
        Credential::ApiKey { value, .. } => {
            request = request.bearer_auth(value);
        }
        Credential::XaiOauth { access_token } | Credential::ClaudeOauth { access_token, .. } => {
            request = request.bearer_auth(access_token);
        }
        Credential::CursorOauth { .. } | Credential::Passthrough => {}
    }
    request
        .body(body.clone())
        .send()
        .await
        .map_err(|error| own_error(error.to_string()))
}

/// Relay an upstream Responses response to the inbound client **verbatim**:
/// preserve the status and forward every upstream header except the framing/
/// hop-by-hop set ([`PASSTHROUGH_STRIP_RESPONSE_HEADERS`]) that axum must
/// recompute for the streamed body. So `content-type` (SSE stays
/// `text/event-stream`, a single JSON body stays `application/json`),
/// `retry-after` (a relayed 429 lets the Codex CLI back off), `x-codex-turn-state`
/// (turn continuity), request ids, and `openai-*` all reach the CLI unchanged. The
/// body bytes stream through unbuffered — no keepalive pings, no SSE parsing, no
/// translation — so the Codex CLI consumes the same bytes the ChatGPT/Codex
/// backend produced.
fn relay_passthrough(upstream: reqwest::Response) -> axum::response::Response {
    let status = upstream.status();
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream.headers() {
        if PASSTHROUGH_STRIP_RESPONSE_HEADERS.contains(&name.as_str()) {
            continue;
        }
        builder = builder.header(name.clone(), value.clone());
    }
    builder
        .body(Body::from_stream(upstream.bytes_stream()))
        .expect("response builder uses valid status and forwarded headers")
        .into_response()
}

/// Send the upstream Responses HTTP request and return the raw response
/// without judging its status. Split out of [`forward_http`] so the account
/// pool path ([`forward_chatgpt_oauth`]) can classify a response for failover
/// before deciding whether to relay, retry, or rotate — while single-account
/// callers still get byte-identical behavior through the [`forward_http`]
/// wrapper below.
async fn http_send(
    state: &AppState,
    route: &Route,
    credential: Credential,
    session_id: Option<&str>,
    upstream_body: &Value,
) -> Result<reqwest::Response, AdapterError> {
    request_builder(state, route, credential, session_id)
        .body(upstream_body.to_string())
        .send()
        .await
        .map_err(|error| own_error(error.to_string()))
}

/// Drive a turn over the HTTP Responses path. The default transport for every
/// provider, and the fallback when the opt-in websocket transport fails to
/// connect (see [`forward`]).
#[allow(clippy::too_many_arguments)]
async fn forward_http(
    state: &AppState,
    route: &Route,
    upstream_body: Value,
    credential: Credential,
    auth: AuthMode,
    client_wants_stream: bool,
    thinking_enabled: bool,
    tool_search_native: bool,
    estimate_input: Option<Arc<Value>>,
    session_id: Option<&str>,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    // Kick off the CPU-bound tiktoken encode on the blocking pool *before* the
    // upstream request so it overlaps that round-trip; the result is not needed
    // until the response stream (and thus message_start) begins. `None` on
    // non-streaming turns and non-tiktoken providers (gated in `forward`).
    let estimate_handle = estimate_input.map(|request| {
        tokio::task::spawn_blocking(move || crate::count_tokens::count_input_tokens_value(&request))
    });
    // Send via the shared helper (extracted so the account-pool path can classify
    // a response before relaying); it wraps the same request_builder + send used
    // above the merge with #112's estimate overlap.
    let upstream = http_send(state, route, credential, session_id, &upstream_body).await?;
    let status = upstream.status();
    if !status.is_success() {
        return Err(mapped_upstream_error(status, upstream, auth).await);
    }
    if client_wants_stream {
        let input_tokens_estimate = match estimate_handle {
            Some(handle) => handle.await.unwrap_or(0),
            None => 0,
        };
        let keepalive = std::time::Duration::from_secs(state.config.server.sse_keepalive_seconds);
        Ok((
            StatusCode::OK,
            stream_response(
                upstream,
                route.model.clone(),
                thinking_enabled,
                tool_search_native,
                input_tokens_estimate,
                keepalive,
            ),
        ))
    } else {
        Ok((
            StatusCode::OK,
            json_response(
                upstream,
                route.model.clone(),
                thinking_enabled,
                tool_search_native,
            )
            .await?,
        ))
    }
}

fn stream_response(
    upstream: reqwest::Response,
    model: String,
    thinking_enabled: bool,
    tool_search_native: bool,
    input_tokens_estimate: u64,
    keepalive: std::time::Duration,
) -> axum::response::Response {
    let bytes = upstream.bytes_stream();
    let parser = SseParser::default();
    let machine = AnthropicSseMachine::new(model, thinking_enabled, tool_search_native)
        .with_input_estimate(input_tokens_estimate);
    let output = stream::unfold((bytes, parser, machine, false), |state| async move {
        let (mut bytes, mut parser, mut machine, mut finished) = state;
        if finished {
            return None;
        }
        loop {
            match bytes.next().await {
                Some(Ok(chunk)) => {
                    let events = parser.push(&String::from_utf8_lossy(&chunk));
                    let data = events
                        .into_iter()
                        .flat_map(|event| machine.apply(event))
                        .collect::<String>();
                    if !data.is_empty() {
                        return Some((
                            Ok::<_, reqwest::Error>(Bytes::from(data)),
                            (bytes, parser, machine, false),
                        ));
                    }
                }
                Some(Err(error)) => return Some((Err(error), (bytes, parser, machine, true))),
                None => {
                    let data = machine.finish().join("");
                    finished = true;
                    if data.is_empty() {
                        return None;
                    }
                    return Some((Ok(Bytes::from(data)), (bytes, parser, machine, finished)));
                }
            }
        }
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

async fn json_response(
    upstream: reqwest::Response,
    model: String,
    thinking_enabled: bool,
    tool_search_native: bool,
) -> Result<axum::response::Response, AdapterError> {
    let body = upstream
        .text()
        .await
        .map_err(|error| own_error(error.to_string()))?;
    let mut machine = AnthropicSseMachine::new(model, thinking_enabled, tool_search_native);
    for event in parse_sse_events(&body) {
        let _ = machine.apply(event);
    }
    Ok((StatusCode::OK, axum::Json(machine.final_json())).into_response())
}

/// Drive a turn over the Codex Responses WebSocket v2 transport (issue #32).
/// Reuses the session's pooled connection and, when the current input is an
/// append-only extension of the previous turn, sends only the delta with
/// `previous_response_id` (the payload-reduction lever). Events are re-encoded
/// through the same [`AnthropicSseMachine`] the HTTP path uses; a rejected
/// handshake is re-shaped exactly like an HTTP upstream error.
#[allow(clippy::too_many_arguments)]
async fn forward_websocket(
    state: &AppState,
    route: &Route,
    pool_key: Option<&str>,
    upstream_body: Value,
    credential: Credential,
    auth: AuthMode,
    client_wants_stream: bool,
    thinking_enabled: bool,
    tool_search_native: bool,
    estimate_input: Option<Arc<Value>>,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let pool_key = pool_key.filter(|key| !key.is_empty());
    let http_url = responses_url(&state.config, &route.provider);
    let ws_url = codex_ws::to_websocket_url(&http_url).map_err(ws_transport_error)?;
    let ctx = WsTurnContext {
        ws_url,
        pool_key,
        provider: &route.provider,
        credential,
        auth,
        signature: codex_continuation::signature(&upstream_body),
        full_input: upstream_body
            .get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        upstream_body,
    };
    tracing::debug!(provider = %route.provider, ws_url = %ctx.ws_url, pool_key = pool_key.unwrap_or(""), "opening codex websocket");

    // Overlap the CPU-bound tiktoken encode with the websocket connect (same
    // rationale as forward_http); its result is only consumed once the event
    // stream begins. If open_ws_turn fails, this handle is dropped un-awaited and
    // forward_http re-spawns its own encode on the fallback path — a rare,
    // off-executor double-encode we accept rather than thread the handle back out
    // through the Err path (which would couple the transport signatures).
    let estimate_handle = estimate_input.map(|request| {
        tokio::task::spawn_blocking(move || crate::count_tokens::count_input_tokens_value(&request))
    });
    let (buffered, events) = open_ws_turn(&ctx).await?;
    if client_wants_stream {
        let input_tokens_estimate = match estimate_handle {
            Some(handle) => handle.await.unwrap_or(0),
            None => 0,
        };
        let keepalive = std::time::Duration::from_secs(state.config.server.sse_keepalive_seconds);
        Ok((
            StatusCode::OK,
            stream_events_response(
                buffered,
                events,
                route.model.clone(),
                thinking_enabled,
                tool_search_native,
                input_tokens_estimate,
                keepalive,
            ),
        ))
    } else {
        Ok((
            StatusCode::OK,
            json_events_response(
                buffered,
                events,
                route.model.clone(),
                thinking_enabled,
                tool_search_native,
            )
            .await,
        ))
    }
}

/// Everything needed to (re)start a websocket turn for a request.
struct WsTurnContext<'a> {
    ws_url: String,
    pool_key: Option<&'a str>,
    provider: &'a str,
    credential: Credential,
    auth: AuthMode,
    signature: String,
    full_input: Vec<Value>,
    upstream_body: Value,
}

/// The first event, peeked off the stream before the websocket response is
/// committed, then replayed ahead of the rest of the channel so the peek costs no
/// event. See [`open_ws_turn`] for why every turn is peeked.
type BufferedEvent = Option<Result<ResponseEvent, CodexWsError>>;

/// Open a websocket turn, applying `previous_response_id` continuation when the
/// connection is reused and the input is an append-only extension. If the backend
/// rejects the replayed id, retry once with the full input on a fresh connection.
///
/// The first event is always peeked before the response is committed, extending
/// the pre-handshake HTTP safety net across the send→first-event window (issue
/// #46). `Turn::stream` only *queues* the frame; the connection reader sends it
/// and produces the first event asynchronously, so a socket that dies between the
/// send and the first event — an idle-eviction race, a backend hiccup, a network
/// blip — would otherwise surface as an error event on an already-committed
/// stream. Peeking lets [`commit_or_fallback`] catch that failure while nothing
/// has reached the client yet and return `Err`, so [`forward`] transparently
/// re-drives the whole turn over HTTP. If instead the backend accepts the frame
/// but never produces a first event, the peek waits up to the reader's idle
/// timeout before falling back — a bounded cost that still never does worse than
/// plain HTTP against the same unresponsive backend. Only once the first event is
/// in hand is the turn under way; a failure after that is genuinely mid-stream and
/// surfaces as a clean error: an Anthropic `error` event for a streaming client
/// ([`stream_events_response`]), a gateway error for a non-streaming one
/// ([`json_events_response`]).
async fn open_ws_turn(
    ctx: &WsTurnContext<'_>,
) -> Result<(BufferedEvent, CodexWsEvents), AdapterError> {
    let (events, used_continuation) = start_ws_turn(ctx, true).await?;
    let (first, events) = peek_first_event(events).await;
    // A rejected previous_response_id arrives before any output: retry once with
    // the full input on a fresh connection, then evaluate that stream instead.
    if used_continuation && matches!(&first, Some(Err(error)) if error.previous_response_missing) {
        tracing::info!("codex previous_response_id rejected; retrying with full input");
        let (events, _) = start_ws_turn(ctx, false).await?;
        let (first, events) = peek_first_event(events).await;
        return commit_or_fallback(first, events);
    }
    commit_or_fallback(first, events)
}

/// Await the first event of a freshly opened turn, returning it alongside the
/// still-live channel so it can be replayed before the remainder of the stream.
async fn peek_first_event(mut events: CodexWsEvents) -> (BufferedEvent, CodexWsEvents) {
    let first = events.recv().await;
    (first, events)
}

/// Decide, from the peeked first event, whether to commit to the websocket
/// response or fall back to HTTP. A delivered first event (`Ok`) means the turn is
/// under way: buffer it for replay and stream the socket. A transport error or an
/// empty stream means nothing ever reached the client, so return `Err` to let
/// [`forward`] re-drive the turn over HTTP transparently — the send→first-event
/// analogue of the pre-handshake fallback. Backend-sent error *events* (a rate
/// limit, a content-policy refusal) arrive as `Ok` and are streamed through rather
/// than retried; only genuine transport failures reach the `Err` arm here.
fn commit_or_fallback(
    first: BufferedEvent,
    events: CodexWsEvents,
) -> Result<(BufferedEvent, CodexWsEvents), AdapterError> {
    match first {
        Some(Ok(event)) => Ok((Some(Ok(event)), events)),
        Some(Err(error)) => Err(ws_transport_error(error)),
        None => Err(own_error(
            "codex websocket closed before any event".to_string(),
        )),
    }
}

/// Rewrite `frame_body` in place for a continuation turn: replace `input` with the
/// decision's delta, set `previous_response_id`, and — when a turn-state token is
/// present — echo it into `client_metadata` as `x-codex-turn-state`. Returns the
/// delta item count (for logging). Pure and unit-tested; the async turn setup that
/// produces the inputs stays in [`start_ws_turn`].
fn apply_continuation(
    frame_body: &mut Value,
    decision: &codex_continuation::Decision,
    turn_state: Option<&str>,
) -> usize {
    if let Some(object) = frame_body.as_object_mut() {
        object.insert("input".to_string(), json!(decision.input_delta));
        object.insert(
            "previous_response_id".to_string(),
            json!(decision.previous_response_id),
        );
        if let Some(turn_state) = turn_state {
            let metadata = object
                .entry("client_metadata".to_string())
                .or_insert_with(|| json!({}));
            // Defensive: a pre-existing non-object `client_metadata` would make
            // `as_object_mut()` return None and silently drop the turn-state token;
            // reset it to an object so the token is always recorded.
            if !metadata.is_object() {
                *metadata = json!({});
            }
            if let Some(metadata) = metadata.as_object_mut() {
                metadata.insert("x-codex-turn-state".to_string(), json!(turn_state));
            }
        }
    }
    decision.input_delta.len()
}

/// Begin a turn on the session's connection and send its frame. When
/// `allow_continuation` and the reused connection's stored state make the input an
/// append-only extension, send only the delta with `previous_response_id`;
/// otherwise send the full input. Returns the event stream and whether the delta
/// path was taken.
async fn start_ws_turn(
    ctx: &WsTurnContext<'_>,
    allow_continuation: bool,
) -> Result<(CodexWsEvents, bool), AdapterError> {
    let headers = websocket_headers(ctx.credential.clone())?;
    let turn = codex_ws::begin(&ctx.ws_url, headers, ctx.pool_key)
        .await
        .map_err(|error| ws_connect_error(error, ctx.auth))?;

    let mut frame_body = ctx.upstream_body.clone();
    let mut used_continuation = false;
    if allow_continuation {
        // Only a reused connection carries stored continuation state; a fresh
        // connection has no chance to continue and is not counted, so the hit/
        // fallback series stay directly comparable (issue #45).
        if let Some(stored) = turn.stored_continuation() {
            match codex_continuation::decide(&stored, &ctx.upstream_body) {
                Some(decision) => {
                    let turn_state = stored
                        .turn_state
                        .as_deref()
                        .or_else(|| turn.handshake_turn_state());
                    let delta_items = apply_continuation(&mut frame_body, &decision, turn_state);
                    used_continuation = true;
                    tracing::debug!(
                        delta_items,
                        "codex websocket continuing with previous_response_id"
                    );
                    crate::metrics::record_continuation_outcome(
                        ctx.provider,
                        crate::metrics::ContinuationOutcome::Hit,
                    );
                }
                None => {
                    // The reused connection's stored transcript was not an
                    // append-only prefix of this input (history rewrite, a changed
                    // non-input field, or normalization drift), so re-send the full
                    // input. Correct, but the payload-trim was missed.
                    tracing::debug!(
                        "codex websocket reused connection but input diverged; re-sending full input"
                    );
                    crate::metrics::record_continuation_outcome(
                        ctx.provider,
                        crate::metrics::ContinuationOutcome::Fallback,
                    );
                }
            }
        }
    }

    let frame = codex_ws::response_create_frame(frame_body);
    let record = codex_ws::RecordPlan {
        signature: ctx.signature.clone(),
        request_input: ctx.full_input.clone(),
    };
    let events = turn
        .stream(&frame, record)
        .await
        .map_err(|error| ws_connect_error(error, ctx.auth))?;
    Ok((events, used_continuation))
}

/// Codex identity + beta-protocol headers for the websocket upgrade. Mirrors the
/// ChatGPT/Codex arm of [`request_builder`] but swaps `OpenAI-Beta` for the
/// websocket protocol value. Only the ChatGPT OAuth credential reaches here
/// (the transport is gated to that backend); other credential shapes still send
/// their bearer so a misconfiguration fails upstream rather than silently
/// unauthenticated.
fn websocket_headers(credential: Credential) -> Result<HeaderMap, AdapterError> {
    let mut headers = HeaderMap::new();
    let mut set = |name: &'static str, value: String| -> Result<(), AdapterError> {
        let value = HeaderValue::from_str(&value).map_err(|error| {
            let message = format!("invalid {name} header: {error}");
            let response = ShuntError::bad_gateway(message.clone()).into_response();
            AdapterError {
                message,
                response: Box::new(response),
            }
        })?;
        headers.insert(name, value);
        Ok(())
    };
    set("openai-beta", codex_ws::WEBSOCKET_BETA_PROTOCOL.to_string())?;
    match credential {
        Credential::ChatGptOAuth {
            access_token,
            account_id,
        } => {
            set("authorization", format!("Bearer {access_token}"))?;
            set("chatgpt-account-id", account_id)?;
            set("originator", "codex_cli_rs".to_string())?;
            set("user-agent", CODEX_USER_AGENT.to_string())?;
            set("version", CODEX_CLIENT_VERSION.to_string())?;
        }
        Credential::ApiKey { value, .. } => set("authorization", format!("Bearer {value}"))?,
        Credential::XaiOauth { access_token } => {
            set("authorization", format!("Bearer {access_token}"))?
        }
        Credential::CursorOauth { access_token } => {
            set("authorization", format!("Bearer {access_token}"))?
        }
        Credential::ClaudeOauth { access_token, .. } => {
            set("authorization", format!("Bearer {access_token}"))?
        }
        Credential::Passthrough => {}
    }
    Ok(headers)
}

fn ws_transport_error(error: CodexWsError) -> AdapterError {
    let response = ShuntError::bad_gateway(error.message.clone()).into_response();
    AdapterError {
        message: error.message,
        response: Box::new(response),
    }
}

/// Map a websocket handshake failure to an [`AdapterError`]. A refused upgrade
/// carries an HTTP status/body, so it re-shapes through the shared
/// [`build_upstream_error`]; a pure transport failure (DNS, TLS, timeout) maps to
/// 502 like a failed HTTP send.
fn ws_connect_error(error: CodexWsError, auth: AuthMode) -> AdapterError {
    match error.status {
        Some(status) => build_upstream_error(status, error.retry_after, error.body, auth),
        None => {
            tracing::warn!(reason = %error.message, "codex websocket transport failure");
            ws_transport_error(error)
        }
    }
}

/// Stream translated events to the client as Anthropic SSE. Mirrors
/// [`stream_response`] but reads from the websocket event channel. By the time
/// this runs the first event has already been delivered (peeked in
/// [`open_ws_turn`], replayed here via `buffered`), so a transport error at this
/// point is genuinely mid-stream: it is surfaced as an Anthropic `error` event so
/// the client sees a reason rather than a silent truncation — an HTTP restart is
/// no longer safe because output has already been streamed.
fn stream_events_response(
    buffered: BufferedEvent,
    events: CodexWsEvents,
    model: String,
    thinking_enabled: bool,
    tool_search_native: bool,
    input_tokens_estimate: u64,
    keepalive: std::time::Duration,
) -> axum::response::Response {
    let machine = AnthropicSseMachine::new(model, thinking_enabled, tool_search_native)
        .with_input_estimate(input_tokens_estimate);
    let output = stream::unfold(
        (buffered, events, machine, false),
        |(mut buffered, mut events, mut machine, finished)| async move {
            if finished {
                return None;
            }
            loop {
                let item = match buffered.take() {
                    Some(item) => Some(item),
                    None => events.recv().await,
                };
                match item {
                    Some(Ok(event)) => {
                        let data = machine.apply(event).into_iter().collect::<String>();
                        if !data.is_empty() {
                            return Some((
                                Ok::<_, std::convert::Infallible>(Bytes::from(data)),
                                (buffered, events, machine, false),
                            ));
                        }
                    }
                    Some(Err(error)) => {
                        return Some((
                            Ok(Bytes::from(ws_error_sse(&error))),
                            (buffered, events, machine, true),
                        ));
                    }
                    None => {
                        let data = machine.finish().join("");
                        if data.is_empty() {
                            return None;
                        }
                        return Some((Ok(Bytes::from(data)), (buffered, events, machine, true)));
                    }
                }
            }
        },
    );

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(crate::keepalive::with_pings(
            output, keepalive,
        )))
        .expect("response builder uses valid status and headers")
        .into_response()
}

/// Collect the full websocket event stream into a single Anthropic message for a
/// non-streaming client. The first event was peeked in [`open_ws_turn`] (a
/// pre-first-event failure already fell back to HTTP), so a transport error here
/// is mid-stream: return a gateway error instead of presenting partial output as
/// a successful response. `buffered` is the replayed first event, if any.
///
/// Note the asymmetry: a mid-stream *transport* error surfaces as a gateway
/// error, but a backend-sent error *event* (arriving as `Ok`, e.g. rate-limit or
/// content-policy) is currently applied by `machine` like any other event and not
/// surfaced as a gateway error — a pre-existing limitation shared with the HTTP
/// `json_response` path, tracked separately.
async fn json_events_response(
    buffered: BufferedEvent,
    mut events: CodexWsEvents,
    model: String,
    thinking_enabled: bool,
    tool_search_native: bool,
) -> axum::response::Response {
    let mut machine = AnthropicSseMachine::new(model, thinking_enabled, tool_search_native);
    let mut buffered = buffered;
    loop {
        let item = match buffered.take() {
            Some(item) => Some(item),
            None => events.recv().await,
        };
        match item {
            Some(Ok(event)) => {
                let _ = machine.apply(event);
            }
            Some(Err(error)) => {
                tracing::warn!(error = %error.message, "codex websocket stream error");
                let message = if error.body.is_empty() {
                    error.message
                } else {
                    error.body
                };
                return ShuntError::bad_gateway(message).into_response();
            }
            None => break,
        }
    }
    (StatusCode::OK, axum::Json(machine.final_json())).into_response()
}

/// Render a websocket transport error as an Anthropic `error` SSE event.
fn ws_error_sse(error: &CodexWsError) -> String {
    let message = if error.body.is_empty() {
        error.message.clone()
    } else {
        error.body.clone()
    };
    let value = map_error_value(&json!({ "message": message }), StatusCode::BAD_GATEWAY);
    format!("event: error\ndata: {value}\n\n")
}

async fn mapped_upstream_error(
    status: StatusCode,
    upstream: reqwest::Response,
    auth: crate::config::AuthMode,
) -> AdapterError {
    // Claude Code backs off on 429 by honoring Retry-After; the header must
    // survive the error re-shaping or the client retries blind.
    let retry_after = upstream
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let text = upstream.text().await.unwrap_or_default();
    build_upstream_error(status, retry_after, text, auth)
}

/// Re-shape an upstream failure (status + body + `retry-after`) into an
/// Anthropic-shaped [`AdapterError`]. Split out of [`mapped_upstream_error`] so
/// both the HTTP path (which reads a `reqwest::Response`) and the websocket path
/// (which surfaces the same fields from a failed handshake) share one mapping.
fn build_upstream_error(
    status: StatusCode,
    retry_after: Option<String>,
    text: String,
    auth: crate::config::AuthMode,
) -> AdapterError {
    tracing::warn!(%status, ?auth, upstream_error_body = %text, "responses upstream error");
    let value =
        if status == StatusCode::UNAUTHORIZED && auth == crate::config::AuthMode::ChatgptOauth {
            json!({"message": "ChatGPT authentication failed; run codex login"})
        } else if status == StatusCode::UNAUTHORIZED && auth == crate::config::AuthMode::XaiOauth {
            json!({"message": "xAI authentication failed; run shunt login xai"})
        } else if status == StatusCode::FORBIDDEN && auth == crate::config::AuthMode::XaiOauth {
            // Usually the subscription tier gate (as on refresh), but this
            // endpoint can also 403 for content policy or model gating — keep
            // the upstream message when there is one and append the tier-gate
            // hint, rather than replacing real context with generic guidance.
            let hint = "if this is the xAI subscription tier gate, re-logging in \
                        will not help — set XAI_API_KEY or upgrade your plan";
            let upstream_message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|value| {
                    value
                        .pointer("/error/message")
                        .or_else(|| value.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .filter(|message| !message.is_empty());
            match upstream_message {
                Some(message) => json!({"message": format!("{message} ({hint})")}),
                None => json!({"message": crate::auth::xai::auth::refresh_error_message(status)}),
            }
        } else {
            serde_json::from_str(&text).unwrap_or_else(|_| json!({"message": text}))
        };
    let shunt_status = crate::model::responses::client_facing_status(status);
    let mut response = (shunt_status, axum::Json(map_error_value(&value, status))).into_response();
    if let Some(retry_after) = retry_after.and_then(|value| value.parse().ok()) {
        response.headers_mut().insert("retry-after", retry_after);
    }
    AdapterError {
        message: format!("upstream responses request failed with {status}"),
        response: Box::new(response),
    }
}

fn own_error(message: String) -> AdapterError {
    let error = ShuntError::bad_gateway(message);
    AdapterError {
        message: "responses adapter failed".to_string(),
        response: Box::new(error.into_response()),
    }
}

/// Codex CLI client identity, mirrored from openai/codex rust-v0.144.1.
///
/// The ChatGPT backend routes newer model slugs (e.g. gpt-5.6-luna, which has
/// `minimal_client_version: 0.144.0`) by client identity and answers
/// "Model not found" — not an entitlement error — when the identity is
/// missing or too old. Per openai/codex#31967 the gate keys on the
/// `originator` + `version` header combination; the `user-agent` is sent for
/// fidelity with Codex, which builds it as
/// `{originator}/{version} ({os} {os_version}; {arch}) {terminal}`
/// (codex-rs/login/src/auth/default_client.rs) and sends the bare CLI
/// version in a `version` header (codex-rs/model-provider-info/src/lib.rs).
/// Bump both together when a new slug requires a newer client version.
const CODEX_USER_AGENT: &str = "codex_cli_rs/0.144.1";
const CODEX_CLIENT_VERSION: &str = "0.144.1";

/// Grok CLI identity, mirrored from the official Grok CLI (via
/// raine/claude-code-proxy `src/providers/grok/client.rs`). The subscription
/// surface (`cli-chat-proxy.grok.com`) gates on these headers: without them it
/// answers as if the caller were an unentitled API client. Sent only with the
/// `XaiOauth` (subscription bearer) credential.
const GROK_CLIENT_IDENTIFIER: &str = "grok-shell";
const GROK_CLIENT_VERSION: &str = "0.2.93";

fn request_builder(
    state: &AppState,
    route: &Route,
    credential: Credential,
    session_id: Option<&str>,
) -> reqwest::RequestBuilder {
    let mut request = state
        .http_client
        .post(responses_url(&state.config, &route.provider))
        .header("content-type", "application/json");
    // `OpenAI-Beta: responses=experimental` is an OpenAI/ChatGPT header; xAI's
    // Responses API doesn't expect it and the reference clients don't send it.
    if !matches!(
        state.config.responses_flavor(&route.provider),
        crate::config::ResponsesFlavor::Xai | crate::config::ResponsesFlavor::Grok
    ) {
        request = request.header("OpenAI-Beta", "responses=experimental");
    }
    match credential {
        // The Responses API is always Bearer-authenticated; the configured
        // api_key_header only governs the Anthropic passthrough adapter.
        Credential::ApiKey { value, .. } => {
            request = request.bearer_auth(value);
        }
        Credential::ChatGptOAuth {
            access_token,
            account_id,
        } => {
            request = request
                .bearer_auth(access_token)
                .header("chatgpt-account-id", account_id)
                .header("originator", "codex_cli_rs")
                .header("user-agent", CODEX_USER_AGENT)
                .header("version", CODEX_CLIENT_VERSION);
            // Session/identity headers the real Codex CLI sends alongside the
            // client identity above (raine/claude-code-proxy build_codex_headers,
            // cross-checked against codex-rs/login/src/auth/default_client.rs).
            // Only sent when a session id is available; xAI/OpenAI-compatible
            // upstreams never reach this branch.
            if let Some(session_id) = session_id.filter(|s| !s.is_empty()) {
                request = request
                    .header("accept", "text/event-stream")
                    .header("session_id", session_id)
                    .header("x-client-request-id", session_id)
                    .header("x-codex-window-id", format!("{session_id}:0"));
            }
        }
        // xAI subscription OAuth: the subscription bearer plus the Grok-CLI
        // identity headers the CLI chat proxy expects (no ChatGPT/Codex
        // account-id/originator headers). `accept: text/event-stream` matches
        // the real Grok CLI; the upstream is always consumed as SSE.
        Credential::XaiOauth { access_token } => {
            request = request
                .bearer_auth(access_token)
                .header("accept", "text/event-stream")
                .header("x-xai-token-auth", "xai-grok-cli")
                .header("x-grok-client-identifier", GROK_CLIENT_IDENTIFIER)
                .header("x-grok-client-version", GROK_CLIENT_VERSION);
        }
        Credential::ClaudeOauth { access_token, .. } => {
            request = request.bearer_auth(access_token);
        }
        // A Responses provider configured with passthrough auth is a
        // misconfiguration; send no credential and let the upstream reject it.
        Credential::CursorOauth { .. } | Credential::Passthrough => {}
    }
    request
}

pub fn responses_url(config: &crate::config::Config, provider: &str) -> String {
    let base = config
        .provider(provider)
        .map(|provider| provider.base_url.as_str())
        .unwrap_or("https://api.openai.com/v1")
        .trim_end_matches('/');
    // The ChatGPT/Codex backend serves the Responses API under /codex/responses;
    // a plain OpenAI-compatible upstream uses /responses.
    if config.is_chatgpt_backend(provider) {
        format!("{base}/codex/responses")
    } else {
        format!("{base}/responses")
    }
}

#[cfg(test)]
pub fn build_test_request(
    state: &AppState,
    route: &Route,
    credential: Credential,
    session_id: Option<&str>,
) -> reqwest::Request {
    request_builder(state, route, credential, session_id)
        .body("{}")
        .build()
        .expect("test request should build")
}

#[derive(Default)]
struct SseParser {
    buffer: String,
}

impl SseParser {
    fn push(&mut self, chunk: &str) -> Vec<ResponseEvent> {
        self.buffer.push_str(chunk);
        let mut out = Vec::new();
        while let Some(index) = self.buffer.find("\n\n") {
            let frame = self.buffer[..index].to_string();
            self.buffer.drain(..index + 2);
            out.extend(parse_sse_events(&(frame + "\n\n")));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use serde_json::{json, Value};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::{
        auth::Credential,
        config::{AuthMode, Config},
        routing::{AdapterKind, Route},
        server::AppState,
    };

    use super::codex_continuation::Decision;
    use super::{apply_continuation, build_test_request, mapped_upstream_error, responses_url};

    /// Serves `body` at `status` from a mock server and returns the resulting
    /// `reqwest::Response`, mirroring the shape `mapped_upstream_error` sees in
    /// production (a response read off the wire, not built in-process).
    async fn upstream_response(
        status: u16,
        body: &str,
        headers: &[(&str, &str)],
    ) -> reqwest::Response {
        let server = MockServer::start().await;
        let mut template = ResponseTemplate::new(status).set_body_string(body.to_string());
        for (name, value) in headers {
            template = template.insert_header(*name, *value);
        }
        Mock::given(method("GET"))
            .and(path("/e"))
            .respond_with(template)
            .mount(&server)
            .await;
        reqwest::Client::new()
            .get(format!("{}/e", server.uri()))
            .send()
            .await
            .expect("mock request should succeed")
    }

    async fn body_json(error: crate::adapters::AdapterError) -> Value {
        let bytes = to_bytes(error.response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable");
        serde_json::from_slice(&bytes).expect("error body should be JSON")
    }

    fn codex_route() -> Route {
        Route {
            provider: "codex".to_string(),
            adapter: AdapterKind::Responses,
            model: "gpt-5.2-codex".to_string(),
            upstream_model: "gpt-5.2-codex".to_string(),
            effort: None,
        }
    }

    #[test]
    fn apply_continuation_sets_delta_previous_id_and_turn_state() {
        // A continuation turn replaces `input` with the delta, sets
        // `previous_response_id`, and echoes the turn-state token.
        let mut frame = json!({"model": "m", "input": ["FULL_INPUT"]});
        let decision = Decision {
            previous_response_id: "resp_1".to_string(),
            input_delta: vec![json!({"type": "message", "role": "user"})],
        };
        let delta_items = apply_continuation(&mut frame, &decision, Some("ts_1"));
        assert_eq!(delta_items, 1);
        assert_eq!(frame["input"], json!([{"type": "message", "role": "user"}]));
        assert_eq!(frame["previous_response_id"], json!("resp_1"));
        assert_eq!(
            frame["client_metadata"]["x-codex-turn-state"],
            json!("ts_1")
        );
    }

    #[test]
    fn apply_continuation_without_turn_state_omits_metadata() {
        // No turn-state token ⇒ no `client_metadata` is synthesized.
        let mut frame = json!({"input": ["FULL_INPUT"]});
        let decision = Decision {
            previous_response_id: "r".to_string(),
            input_delta: vec![json!("a"), json!("b")],
        };
        let delta_items = apply_continuation(&mut frame, &decision, None);
        assert_eq!(delta_items, 2);
        assert_eq!(frame["input"], json!(["a", "b"]));
        assert_eq!(frame["previous_response_id"], json!("r"));
        assert!(frame.get("client_metadata").is_none());
    }

    #[test]
    fn apply_continuation_overwrites_non_object_client_metadata() {
        // A pre-existing non-object `client_metadata` is reset to an object so the
        // turn-state token is recorded rather than silently dropped.
        let mut frame = json!({"input": [], "client_metadata": "not-an-object"});
        let decision = Decision {
            previous_response_id: "r".to_string(),
            input_delta: vec![json!("x")],
        };
        apply_continuation(&mut frame, &decision, Some("ts"));
        assert_eq!(frame["client_metadata"]["x-codex-turn-state"], json!("ts"));
    }

    #[test]
    fn apply_continuation_merges_into_existing_client_metadata() {
        // The turn-state token is inserted alongside pre-existing metadata keys,
        // not clobbering them (the `or_insert_with` existing-object path).
        let mut frame = json!({"input": [], "client_metadata": {"existing": "keep"}});
        let decision = Decision {
            previous_response_id: "r".to_string(),
            input_delta: vec![json!("x")],
        };
        apply_continuation(&mut frame, &decision, Some("ts"));
        assert_eq!(frame["client_metadata"]["existing"], json!("keep"));
        assert_eq!(frame["client_metadata"]["x-codex-turn-state"], json!("ts"));
    }

    #[test]
    fn builds_codex_url_and_headers_without_sending() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();

        let request = build_test_request(
            &state,
            &codex_route(),
            Credential::ChatGptOAuth {
                access_token: "access-token".to_string(),
                account_id: "account-id".to_string(),
            },
            None,
        );

        assert_eq!(
            request.url().as_str(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            request.headers().get("authorization").unwrap(),
            "Bearer access-token"
        );
        assert_eq!(
            request.headers().get("chatgpt-account-id").unwrap(),
            "account-id"
        );
        assert_eq!(request.headers().get("originator").unwrap(), "codex_cli_rs");
        assert_eq!(
            request.headers().get("user-agent").unwrap(),
            super::CODEX_USER_AGENT
        );
        assert_eq!(
            request.headers().get("version").unwrap(),
            super::CODEX_CLIENT_VERSION
        );
        assert_eq!(
            request.headers().get("OpenAI-Beta").unwrap(),
            "responses=experimental"
        );
        // No session id was supplied: the session/identity headers must not
        // be sent, since a fabricated value would be worse than omitting them.
        assert!(request.headers().get("session_id").is_none());
        assert!(request.headers().get("x-client-request-id").is_none());
        assert!(request.headers().get("x-codex-window-id").is_none());
        assert!(request.headers().get("accept").is_none());
    }

    #[test]
    fn forwards_session_headers_on_codex_backend_when_session_id_present() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();

        let request = build_test_request(
            &state,
            &codex_route(),
            Credential::ChatGptOAuth {
                access_token: "access-token".to_string(),
                account_id: "account-id".to_string(),
            },
            Some("session-123"),
        );

        assert_eq!(
            request.headers().get("accept").unwrap(),
            "text/event-stream"
        );
        assert_eq!(request.headers().get("session_id").unwrap(), "session-123");
        assert_eq!(
            request.headers().get("x-client-request-id").unwrap(),
            "session-123"
        );
        assert_eq!(
            request.headers().get("x-codex-window-id").unwrap(),
            "session-123:0"
        );
    }

    #[test]
    fn omits_session_headers_when_session_id_is_empty_string() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();

        let request = build_test_request(
            &state,
            &codex_route(),
            Credential::ChatGptOAuth {
                access_token: "access-token".to_string(),
                account_id: "account-id".to_string(),
            },
            Some(""),
        );

        assert!(request.headers().get("accept").is_none());
        assert!(request.headers().get("session_id").is_none());
        assert!(request.headers().get("x-client-request-id").is_none());
        assert!(request.headers().get("x-codex-window-id").is_none());
    }

    #[test]
    fn builds_openai_responses_url() {
        assert_eq!(
            responses_url(&Config::default(), "openai"),
            "https://api.openai.com/v1/responses"
        );
    }

    fn xai_route() -> Route {
        Route {
            provider: "xai".to_string(),
            adapter: AdapterKind::Responses,
            model: "grok-4.3".to_string(),
            upstream_model: "grok-4.3".to_string(),
            effort: None,
        }
    }

    fn grok_route() -> Route {
        Route {
            provider: "grok".to_string(),
            adapter: AdapterKind::Responses,
            model: "grok-4.5".to_string(),
            upstream_model: "grok-4.5".to_string(),
            effort: None,
        }
    }

    #[test]
    fn builds_grok_oauth_request_with_cli_identity_headers() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();

        let request = build_test_request(
            &state,
            &grok_route(),
            Credential::XaiOauth {
                access_token: "xai-access".to_string(),
            },
            Some("session-123"),
        );

        // The subscription OAuth path targets the Grok CLI chat proxy, not
        // api.x.ai, and carries the Grok-CLI identity headers it gates on.
        assert_eq!(
            request.url().as_str(),
            "https://cli-chat-proxy.grok.com/v1/responses"
        );
        assert_eq!(
            request.headers().get("authorization").unwrap(),
            format!("Bearer {}", "xai-access").as_str()
        );
        assert_eq!(
            request.headers().get("x-xai-token-auth").unwrap(),
            "xai-grok-cli"
        );
        assert_eq!(
            request.headers().get("x-grok-client-identifier").unwrap(),
            "grok-shell"
        );
        assert_eq!(
            request.headers().get("x-grok-client-version").unwrap(),
            "0.2.93"
        );
        assert_eq!(
            request.headers().get("accept").unwrap(),
            "text/event-stream"
        );
        // No ChatGPT/Codex headers and no OpenAI-Beta for the xai flavor, even
        // when a session id is present on the request.
        assert!(request.headers().get("chatgpt-account-id").is_none());
        assert!(request.headers().get("originator").is_none());
        assert!(request.headers().get("user-agent").is_none());
        assert!(request.headers().get("version").is_none());
        assert!(request.headers().get("OpenAI-Beta").is_none());
        assert!(request.headers().get("session_id").is_none());
        assert!(request.headers().get("x-client-request-id").is_none());
        assert!(request.headers().get("x-codex-window-id").is_none());
    }

    #[test]
    fn builds_xai_api_key_request_bearer_only_without_cli_headers() {
        let state = AppState::new(Config::default(), reqwest::Client::new()).unwrap();

        let request = build_test_request(
            &state,
            &xai_route(),
            Credential::ApiKey {
                value: "xai-key".to_string(),
                header: crate::config::ApiKeyHeader::Bearer,
            },
            None,
        );

        // The API-key path stays on the developer API and sends the bearer
        // only — no Grok-CLI identity headers, no OpenAI-Beta (xai flavor).
        assert_eq!(request.url().as_str(), "https://api.x.ai/v1/responses");
        assert_eq!(
            request.headers().get("authorization").unwrap(),
            format!("Bearer {}", "xai-key").as_str()
        );
        assert!(request.headers().get("x-xai-token-auth").is_none());
        assert!(request.headers().get("x-grok-client-identifier").is_none());
        assert!(request.headers().get("x-grok-client-version").is_none());
        assert!(request.headers().get("OpenAI-Beta").is_none());
    }

    #[tokio::test]
    async fn maps_401_to_xai_auth_message_for_xai_oauth() {
        let upstream = upstream_response(401, "{}", &[]).await;
        let error =
            mapped_upstream_error(StatusCode::UNAUTHORIZED, upstream, AuthMode::XaiOauth).await;
        assert_eq!(error.response.status(), StatusCode::UNAUTHORIZED);
        let body = body_json(error).await;
        assert_eq!(
            body["error"]["message"],
            "xAI authentication failed; run shunt login xai"
        );
    }

    #[tokio::test]
    async fn maps_403_to_xai_tier_gate_message_for_xai_oauth() {
        // A live-API 403 without a usable upstream message falls back to the
        // refresh path's tier-gate guidance: 403 kept (not 502), points at
        // XAI_API_KEY, never suggests a re-login.
        let upstream = upstream_response(403, "forbidden", &[]).await;
        let error =
            mapped_upstream_error(StatusCode::FORBIDDEN, upstream, AuthMode::XaiOauth).await;
        assert_eq!(error.response.status(), StatusCode::FORBIDDEN);
        let body = body_json(error).await;
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("tier gate"));
        assert!(message.contains("XAI_API_KEY"));
        assert!(!message.contains("run shunt login xai"));
    }

    #[tokio::test]
    async fn xai_403_preserves_upstream_message_and_appends_tier_hint() {
        // A 403 can also mean content policy or model gating — the upstream
        // message must survive, with the tier-gate possibility as a hint.
        let upstream = upstream_response(
            403,
            r#"{"error": {"message": "model grok-4.5 is not enabled for this account"}}"#,
            &[],
        )
        .await;
        let error =
            mapped_upstream_error(StatusCode::FORBIDDEN, upstream, AuthMode::XaiOauth).await;
        assert_eq!(error.response.status(), StatusCode::FORBIDDEN);
        let body = body_json(error).await;
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("model grok-4.5 is not enabled for this account"));
        assert!(message.contains("XAI_API_KEY"));
    }

    #[tokio::test]
    async fn maps_403_to_permission_error_for_other_auth_modes() {
        // Outside the xAI tier-gate special case, a 403 is still a real
        // "authenticated but not allowed" signal and must reach the client
        // as its own status/type rather than a generic 502 `api_error`.
        let upstream = upstream_response(403, "forbidden", &[]).await;
        let error = mapped_upstream_error(StatusCode::FORBIDDEN, upstream, AuthMode::ApiKey).await;
        assert_eq!(error.response.status(), StatusCode::FORBIDDEN);
        let body = body_json(error).await;
        assert_eq!(body["error"]["type"], "permission_error");
    }

    #[tokio::test]
    async fn maps_401_to_chatgpt_auth_message_for_chatgpt_oauth() {
        let upstream = upstream_response(401, "{}", &[]).await;
        let error =
            mapped_upstream_error(StatusCode::UNAUTHORIZED, upstream, AuthMode::ChatgptOauth).await;
        assert_eq!(error.response.status(), StatusCode::UNAUTHORIZED);
        let body = body_json(error).await;
        assert_eq!(
            body["error"]["message"],
            "ChatGPT authentication failed; run codex login"
        );
    }

    #[tokio::test]
    async fn preserves_upstream_503_status_and_type_instead_of_bad_gateway() {
        // A real upstream 503 must reach the client as 503 `api_error`, not
        // flattened to a generic 502 that hides the actual signal.
        let upstream = upstream_response(503, "service unavailable", &[]).await;
        let error =
            mapped_upstream_error(StatusCode::SERVICE_UNAVAILABLE, upstream, AuthMode::ApiKey)
                .await;
        assert_eq!(error.response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(error).await;
        assert_eq!(body["error"]["type"], "api_error");
    }

    #[tokio::test]
    async fn maps_529_to_overloaded_error() {
        // Claude Code backs off and retries on 529 `overloaded_error`; folding
        // it into a generic 502 would suppress that retry path.
        let upstream = upstream_response(529, "{}", &[]).await;
        let error = mapped_upstream_error(
            StatusCode::from_u16(529).unwrap(),
            upstream,
            AuthMode::ApiKey,
        )
        .await;
        assert_eq!(error.response.status().as_u16(), 529);
        let body = body_json(error).await;
        assert_eq!(body["error"]["type"], "overloaded_error");
    }

    #[tokio::test]
    async fn maps_413_to_request_too_large() {
        let upstream = upstream_response(413, "{}", &[]).await;
        let error =
            mapped_upstream_error(StatusCode::PAYLOAD_TOO_LARGE, upstream, AuthMode::ApiKey).await;
        assert_eq!(error.response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = body_json(error).await;
        assert_eq!(body["error"]["type"], "request_too_large");
    }

    #[tokio::test]
    async fn passes_401_429_and_400_through_unchanged() {
        let upstream = upstream_response(400, "{}", &[]).await;
        let error =
            mapped_upstream_error(StatusCode::BAD_REQUEST, upstream, AuthMode::ApiKey).await;
        assert_eq!(error.response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(error).await;
        assert_eq!(body["error"]["type"], "invalid_request_error");

        let upstream = upstream_response(401, "{}", &[]).await;
        let error =
            mapped_upstream_error(StatusCode::UNAUTHORIZED, upstream, AuthMode::ApiKey).await;
        assert_eq!(error.response.status(), StatusCode::UNAUTHORIZED);
        let body = body_json(error).await;
        assert_eq!(body["error"]["type"], "authentication_error");

        let upstream = upstream_response(429, "{}", &[]).await;
        let error =
            mapped_upstream_error(StatusCode::TOO_MANY_REQUESTS, upstream, AuthMode::ApiKey).await;
        assert_eq!(error.response.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = body_json(error).await;
        assert_eq!(body["error"]["type"], "rate_limit_error");
    }

    #[tokio::test]
    async fn preserves_retry_after_header_on_429() {
        let upstream = upstream_response(429, "{}", &[("retry-after", "7")]).await;
        let error =
            mapped_upstream_error(StatusCode::TOO_MANY_REQUESTS, upstream, AuthMode::ApiKey).await;
        assert_eq!(error.response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error.response.headers().get("retry-after").unwrap(), "7");
    }

    /// `commit_or_fallback` classifies the peeked first event: a delivered event
    /// commits to the websocket (buffered for replay), while a transport error or
    /// an empty stream returns `Err` so [`super::forward`] re-drives over HTTP. The
    /// empty-stream arm is unreachable from the integration mocks (which always
    /// send an event or a transport error before closing), so it is exercised here.
    #[test]
    fn commit_or_fallback_classifies_the_peeked_first_event() {
        use super::{commit_or_fallback, CodexWsError, ResponseEvent};
        use tokio::sync::mpsc;

        // A delivered first event commits: it is buffered for replay and the
        // channel is handed back intact.
        let (_tx, rx) = mpsc::unbounded_channel();
        let event = ResponseEvent {
            event: Some("response.created".to_string()),
            data: Value::Null,
        };
        let (buffered, _events) =
            commit_or_fallback(Some(Ok(event)), rx).expect("a delivered event commits");
        assert!(
            matches!(buffered, Some(Ok(_))),
            "the first event is buffered for replay"
        );

        // A transport error before the first event falls back to HTTP.
        let (_tx, rx) = mpsc::unbounded_channel();
        let error = CodexWsError {
            status: None,
            retry_after: None,
            body: String::new(),
            message: "socket dropped before first event".to_string(),
            previous_response_missing: false,
        };
        assert!(
            commit_or_fallback(Some(Err(error)), rx).is_err(),
            "a pre-first-event transport error falls back to HTTP"
        );

        // An empty stream (channel closed before any event) also falls back.
        let (_tx, rx) = mpsc::unbounded_channel();
        assert!(
            commit_or_fallback(None, rx).is_err(),
            "an empty stream falls back to HTTP"
        );
    }
}
