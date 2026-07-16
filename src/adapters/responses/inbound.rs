//! Raw inbound Codex/OpenAI Responses passthrough served by `[server.codex_endpoint]`.

use std::time::Duration;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Response, StatusCode},
    response::IntoResponse,
};

use crate::{
    adapters::AdapterError,
    auth::{self, resolve_credential, Credential},
    config::AccountConfig,
    routing::Route,
    server::AppState,
};

use super::{
    error::own_error,
    pool::{
        classify_first, classify_retry, force_refresh_or_cooldown, resolve_or_cooldown,
        with_account_header, FirstOutcome, RetryOutcome,
    },
    request::responses_url,
};

/// Entry point for the inbound `[server.codex_endpoint]` passthrough. Gathers the
/// target provider's pooled accounts (explicit `[[accounts]]` or a store scan)
/// and drives the passthrough over the pool; falls back to the single default
/// `~/.codex/auth.json` credential when no pooled accounts exist — mirroring the
/// outbound [`forward`] `chatgpt_oauth` branch so a single-account user keeps
/// working without configuring a pool.
pub(crate) async fn forward_codex_inbound(
    state: AppState,
    route: Route,
    pool_key: Option<String>,
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
    forward_codex_passthrough(state, route, accounts, pool_key, passthrough_headers, body).await
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
///
/// `pool_key` is the session-sticky selection key, already namespaced with the
/// authenticated inbound client by the caller (`codex_endpoint::pool_sticky_key`),
/// so replaying another client's `session-id` cannot steer its session onto a
/// chosen pool account.
async fn forward_codex_passthrough(
    state: AppState,
    route: Route,
    accounts_config: Vec<AccountConfig>,
    pool_key: Option<String>,
    passthrough_headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
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
    // Codex quota is display-only: this endpoint preserves cooldown-based
    // selection even when response headers populate the admin dashboard.
    let order = state.accounts.select_order_cooldown(
        &route.provider,
        &accounts_config,
        pool_key.as_deref(),
    );
    let mut last_response: Option<reqwest::Response> = None;

    for index in order {
        let account = &accounts_config[index];

        // Resolve the account's credential under its per-account refresh lock
        // (shared with forward_chatgpt_oauth); a resolution failure cools it
        // down and rotates to the next account.
        let credential = match resolve_or_cooldown(&state, &route, account).await {
            Some(credential) => credential,
            None => continue,
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

        state
            .accounts
            .note_codex_quota(&route.provider, &account.name, upstream.headers());
        match classify_first(&state, &route, account, upstream) {
            // Success or a non-failover 4xx (e.g. 400): the account is fine, so
            // relay the upstream response verbatim — a passthrough client expects
            // the raw Responses body, error or not — and never rotate.
            FirstOutcome::Relay(upstream) => {
                let status = upstream.status();
                state.accounts.mark_healthy(&route.provider, &account.name);
                return Ok((
                    status,
                    with_account_header(relay_passthrough(upstream), &account.name),
                ));
            }
            FirstOutcome::Rotate(upstream) => {
                last_response = Some(upstream);
            }
            FirstOutcome::NeedRefresh(upstream) => {
                // Force-refresh the account's stored credential (shared with
                // forward_chatgpt_oauth); a `token_env` account or a refresh
                // failure cools it down and rotates instead.
                let retry_credential =
                    match force_refresh_or_cooldown(&state, &route, account, &credential).await {
                        Some(credential) => credential,
                        None => {
                            last_response = Some(upstream);
                            continue;
                        }
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
                state
                    .accounts
                    .note_codex_quota(&route.provider, &account.name, retry.headers());
                match classify_retry(&state, &route, account, retry) {
                    RetryOutcome::Relay(retry) => {
                        let retry_status = retry.status();
                        state.accounts.mark_healthy(&route.provider, &account.name);
                        return Ok((
                            retry_status,
                            with_account_header(relay_passthrough(retry), &account.name),
                        ));
                    }
                    RetryOutcome::Rotate(retry) => {
                        last_response = Some(retry);
                    }
                }
            }
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
/// the default shunt client-token header and the internal `x-shunt-inbound-client`
/// label (a client must never leak or spoof them — matches `proxy::check_inbound_auth`),
/// framing headers the HTTP client recomputes, and `accept-encoding` (dropped so the
/// upstream returns an uncompressed body that [`relay_passthrough`] streams through
/// unchanged). Names compare lowercase — `http` normalizes them.
const PASSTHROUGH_STRIP_REQUEST_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "authorization",
    "chatgpt-account-id",
    "accept-encoding",
    // The default shunt client-token header (`config::default_auth_header`). Always
    // stripped — even on an ungated endpoint (no `[server.auth]`), or one using a
    // custom auth header — so the documented guarantee that the shunt token never
    // reaches the Codex backend holds unconditionally. A non-default configured
    // header is additionally stripped via the `token_header` argument below.
    "x-shunt-token",
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
