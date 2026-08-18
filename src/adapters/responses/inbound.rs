//! Raw inbound Codex/OpenAI Responses passthrough served by `[server.codex_endpoint]`.

use std::time::Duration;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Response, StatusCode},
    response::IntoResponse,
};

use crate::{
    adapters::AdapterError,
    auth::{self, resolve_credential, slots::ShuntCredentials, Credential},
    config::AccountConfig,
    routing::Route,
    server::AppState,
    upstream_timeout::SendError,
};

use super::{
    error::own_error,
    pool::{
        admit_and_resolve, classify_first, classify_retry, force_refresh_or_cooldown,
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
    // credential (below). Every slot shunt reserves — the client-token header,
    // the admin-token header, and the internal client label — is removed via
    // the shared enumeration in `auth::slots`; credential + framing headers are
    // handled in passthrough_request_headers / passthrough_send.
    let passthrough_headers =
        passthrough_request_headers(&client_headers, ShuntCredentials::from_state(&state));

    let provider = state
        .config
        .provider(&route.provider)
        .ok_or_else(|| own_error(format!("unknown provider {}", route.provider)))?;
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
    let observed_account = match &credential {
        Credential::ChatGptOAuth { account_id, .. } => Some(AccountConfig {
            name: "local-codex".to_string(),
            uuid: Some(account_id.clone()),
            ..Default::default()
        }),
        _ => None,
    };
    let upstream = passthrough_send(&state, &route, credential, &passthrough_headers, &body)
        .await
        .map_err(send_error)?;
    if let Some(account) = &observed_account {
        state
            .accounts
            .note_codex_quota(&route.provider, account, upstream.headers());
    }
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
    // Recorded x-codex-* quota windows feed selection here exactly as on the
    // translating outbound path: near-quota accounts rotate proactively and
    // available accounts order by burn-rate headroom (issue #195). The
    // passthrough body's model is a label only (no fable-scoped Codex window).
    let order = state.accounts.select_order(
        &route.provider,
        &accounts_config,
        pool_key.as_deref(),
        Some(route.upstream_model.as_str()),
        state.config.server.pool.as_ref(),
    );
    let ramp_initial = state.config.storm_ramp_initial();
    let candidates = order.len();
    let mut last_response: Option<reqwest::Response> = None;

    for (position, index) in order.into_iter().enumerate() {
        let account = &accounts_config[index];

        // Storm-control admission + credential resolution shared with the
        // translating outbound path (issue #195, `admit_and_resolve`): a
        // saturated identity or failed auth rotates to the next candidate; on
        // a relayed success the guard moves into the response body
        // (`with_admission`) so the slot stays held until the stream finishes.
        let Some((admission, credential)) =
            admit_and_resolve(&state, &route, account, ramp_initial, position, candidates).await
        else {
            continue;
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
            Err(error @ SendError::Timeout) => return Err(send_error(error)),
            Err(SendError::Transport(error)) => {
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
                    "inbound passthrough upstream request failed"
                );
                continue;
            }
        };

        state
            .accounts
            .note_codex_quota(&route.provider, account, upstream.headers());
        match classify_first(&state, &route, account, upstream) {
            // Success or a non-failover 4xx (e.g. 400): the account is fine, so
            // relay the upstream response verbatim — a passthrough client expects
            // the raw Responses body, error or not — and never rotate.
            FirstOutcome::Relay(upstream) => {
                let status = upstream.status();
                // Relayed 4xx bodies pass through verbatim, but only a real
                // success grows the storm-control allowance.
                state
                    .accounts
                    .mark_healthy(&route.provider, account, status.is_success());
                return Ok((
                    status,
                    crate::adapters::with_admission(
                        with_account_header(relay_passthrough(upstream), &account.name),
                        admission,
                    ),
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
                    Err(error @ SendError::Timeout) => return Err(send_error(error)),
                    Err(SendError::Transport(error)) => {
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
                            "inbound passthrough refresh retry failed"
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
                        state.accounts.mark_healthy(
                            &route.provider,
                            account,
                            retry_status.is_success(),
                        );
                        return Ok((
                            retry_status,
                            crate::adapters::with_admission(
                                with_account_header(relay_passthrough(retry), &account.name),
                                admission,
                            ),
                        ));
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
/// framing headers the HTTP client recomputes, and `accept-encoding` (dropped so the
/// upstream returns an uncompressed body that [`relay_passthrough`] streams through
/// unchanged). Names compare lowercase — `http` normalizes them.
///
/// The slots shunt *reserves* — `x-shunt-token`, `x-shunt-admin-token`,
/// `x-shunt-inbound-client`, and whatever `[server.auth]`/`[server.admin]` are
/// configured to — are deliberately **not** listed here: they are owned by
/// `auth::slots::ShuntCredentials::strip_reserved_slots`, which
/// [`passthrough_request_headers`] applies after this list. One owner, or the
/// two lists drift — which is exactly how the admin-token header came to be
/// relayed to the ChatGPT backend.
const PASSTHROUGH_STRIP_REQUEST_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "authorization",
    "chatgpt-account-id",
    // `Config::validate` (`ConfigError::CodexEndpointWrongAuth`) rejects any
    // `[server.codex_endpoint]` whose provider is not `auth = "chatgpt_oauth"`,
    // and that backend authenticates solely via `Authorization: Bearer` +
    // `chatgpt-account-id`, both injected per pool account in
    // [`passthrough_send`]. No inbound `x-api-key` value can therefore ever be a
    // valid credential for this upstream — stripping it unconditionally cannot
    // break a legitimate relay, and forwarding it would leak a caller's secret
    // (e.g. an Anthropic key from Claude Code's `apiKeyHelper`, which populates
    // both `Authorization` and `x-api-key` with the same value) to a third party.
    "x-api-key",
    "accept-encoding",
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
/// re-injected per selected pool account in [`passthrough_send`]; framing/hop-by-hop
/// headers are recomputed by the HTTP client. Everything else — `originator`,
/// `version`, `user-agent`, `OpenAI-Beta`, `session-id`, `thread-id`, `x-codex-*`,
/// `content-type`, `accept` — passes through verbatim, so the Codex CLI's real
/// client identity (its actual version, not a shunt-synthesized one) reaches the
/// backend and model version gating behaves exactly as it would against ChatGPT.
///
/// This is forward site 3 of the enumeration in `auth::slots`: it relays the
/// caller's headers verbatim, so the mirror invariant has to be enforced here
/// explicitly rather than falling out of an allowlist. `strip_reserved_slots`
/// runs last and clears every slot shunt reserves by name — including
/// `x-shunt-admin-token` and any configured `[server.admin] header`, which this
/// path used to relay to the ChatGPT backend even though the admin surface
/// authenticates on it.
pub(crate) fn passthrough_request_headers(
    client: &HeaderMap,
    credentials: ShuntCredentials<'_>,
) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(client.len());
    for (name, value) in client.iter() {
        if PASSTHROUGH_STRIP_REQUEST_HEADERS.contains(&name.as_str()) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    credentials.strip_reserved_slots(&mut out);
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
) -> Result<reqwest::Response, SendError<reqwest::Error>> {
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
        Credential::XaiOauth { access_token }
        | Credential::ClaudeOauth { access_token, .. }
        | Credential::GoogleOauth { access_token, .. } => {
            request = request.bearer_auth(access_token);
        }
        // Send nothing rather than bearer an off-origin subscription token:
        // neither an Antigravity nor a Kimi credential can legitimately reach a
        // Responses upstream (validation pins them to `kind = "antigravity"`
        // and `kind = "anthropic"` respectively), so the defensive arm fails
        // closed.
        Credential::CursorOauth { .. }
        | Credential::KimiOauth { .. }
        | Credential::AntigravityOauth { .. }
        | Credential::Passthrough => {}
    }
    crate::upstream_timeout::wait(
        state.config.server.timeouts.upstream_ttfb_ms,
        request.body(body.clone()).send(),
    )
    .await
}

fn send_error(error: SendError<reqwest::Error>) -> AdapterError {
    error.into_adapter_error(|error| own_error(error.to_string()))
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

#[cfg(test)]
mod tests {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::{config::Config, routing::AdapterKind};

    use super::*;

    fn codex_route() -> Route {
        Route {
            provider: "codex".to_string(),
            adapter: AdapterKind::Responses,
            model: "gpt-5.2-codex".to_string(),
            upstream_model: "gpt-5.2-codex".to_string(),
            effort: None,
            service_tier: None,
        }
    }

    #[tokio::test]
    async fn antigravity_oauth_sends_no_credential_on_the_passthrough_path() {
        // Config validation pins `antigravity_oauth` to `kind = "antigravity"`,
        // so this arm is unreachable in a valid config — but if it were ever
        // reached, it must fail closed rather than bearer an off-origin
        // subscription token onto the Codex passthrough upstream.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        let mut config = Config::default();
        config.providers.get_mut("codex").unwrap().base_url = server.uri();
        let state = AppState::new(config, reqwest::Client::new()).unwrap();

        passthrough_send(
            &state,
            &codex_route(),
            Credential::AntigravityOauth {
                access_token: "antigravity-token".to_string(),
                project_id: "proj-1".to_string(),
            },
            &HeaderMap::new(),
            &Bytes::from_static(b"{}"),
        )
        .await
        .unwrap();

        let requests = server
            .received_requests()
            .await
            .expect("mock records requests");
        assert_eq!(requests.len(), 1);
        let headers = &requests[0].headers;
        assert!(headers.get("authorization").is_none());
        assert!(headers.get("x-api-key").is_none());
    }
}
