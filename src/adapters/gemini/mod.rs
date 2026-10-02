//! Gemini adapter implementation for Google Code Assist / Gemini endpoints.

mod sse;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderValue, Response, StatusCode, Uri},
    response::IntoResponse,
};
use futures_util::StreamExt;
use serde_json::Value;

use crate::{
    accounts::CountAttempt,
    adapters::{Adapter, AdapterError, AdapterFuture},
    auth::{
        antigravity::{auth::inference_base_url, catalog::catalog_ids},
        resolve_credential, Credential,
    },
    config::AuthMode,
    model::antigravity_request::{
        antigravity_model_needs_catalog, antigravity_request_id, antigravity_session_id,
        antigravity_upstream_model_with, wrap_antigravity_envelope, AntigravityCatalog,
    },
    model::gemini::{map_gemini_error, GeminiSseMachine},
    model::gemini_request::{translate_request_for_model, wrap_code_assist_envelope},
    request::RequestBody,
    routing::Route,
    server::AppState,
};

use self::sse::{Decoder as GeminiSseDecoder, Item as GeminiSseItem};

const MAX_GEMINI_UNARY_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

pub struct GeminiAdapter;

impl Adapter for GeminiAdapter {
    fn forward<'a>(
        &'a self,
        state: AppState,
        route: Route,
        uri: &'a Uri,
        headers: &'a HeaderMap,
        body: RequestBody,
        // Both bounds apply to the whole-body reads in `forward_single`: the
        // error body read before `map_gemini_error` and the non-streaming
        // success body. A streaming turn is relayed as it arrives, so its
        // bounds fall to `routing::serve`'s collector on the relayed body.
        bounds: crate::adapters::ResponseBounds,
    ) -> AdapterFuture<'a> {
        Box::pin(async move { forward(state, route, uri, headers, body, bounds).await })
    }
}

/// The Antigravity inference URL for a provider configured with `base_url`.
///
/// Split out so the production redirect is unit-testable without a live
/// request: `inference_base_url` carries the "production does not serve
/// Antigravity inference" rule that onboarding already applies.
fn antigravity_endpoint(base_url: &str, method: &str) -> String {
    let base_url = inference_base_url(base_url);
    format!("{base_url}/v1internal:{method}")
}

/// Carry the effort tier a `-tiered` catalog id does not name into the request
/// body, where the backend reads it.
///
/// Additive on purpose: `thinkingBudget` is already translated from the
/// client's `thinking` block and the backend accepts both fields together, so
/// the level joins the existing `thinkingConfig` rather than replacing it.
///
/// The one case it must not join is a budget of `0`, which is how
/// [`crate::model::gemini_request`] renders an explicitly *disabled* `thinking`
/// block. Writing a level there would send "do not think" and "think at
/// medium" in one object — and if the backend gives the level precedence, a
/// client that opted out of reasoning is billed for it anyway. A configured
/// `effort` is dropped here too, while a suffixed id (`…-flash-high`) would
/// still carry it in the id: an accepted asymmetry, because the client's
/// explicit opt-out is the stronger signal in the one place the backend can
/// honour it.
fn set_thinking_level(inner_req: &mut Value, level: &str) {
    if inner_req
        .pointer("/generationConfig/thinkingConfig/thinkingBudget")
        .and_then(Value::as_u64)
        == Some(0)
    {
        return;
    }
    let Some(object) = inner_req.as_object_mut() else {
        return;
    };
    let generation_config = object
        .entry("generationConfig")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Some(generation_config) = generation_config.as_object_mut() else {
        return;
    };
    let thinking_config = generation_config
        .entry("thinkingConfig")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Some(thinking_config) = thinking_config.as_object_mut() {
        thinking_config.insert(
            "thinkingLevel".to_string(),
            Value::String(level.to_string()),
        );
    }
}

fn append_sse_events(events: Vec<crate::model::gemini::SseEvent>, output: &mut Vec<u8>) {
    for event in events {
        let formatted = format!("event: {}\ndata: {}\n\n", event.event, event.data);
        output.extend_from_slice(formatted.as_bytes());
    }
}

fn append_protocol_error(message: impl Into<String>, output: &mut Vec<u8>) {
    append_sse_events(
        vec![crate::model::gemini::SseEvent {
            event: "error".to_string(),
            data: serde_json::json!({
                "type": "error",
                "error": {"type": "api_error", "message": message.into()}
            }),
        }],
        output,
    );
}

fn local_gemini_error(message: impl Into<String>) -> AdapterError {
    let message = message.into();
    let body = serde_json::json!({
        "type": "error",
        "error": {"type": "api_error", "message": message}
    });
    AdapterError {
        message,
        response: Box::new((StatusCode::BAD_GATEWAY, axum::Json(body)).into_response()),
        failure: None,
    }
}

fn embedded_gemini_error(data: Value) -> AdapterError {
    let error_type = data
        .pointer("/error/type")
        .and_then(Value::as_str)
        .unwrap_or("api_error");
    let status = if error_type == "rate_limit_error" {
        StatusCode::TOO_MANY_REQUESTS
    } else {
        StatusCode::BAD_GATEWAY
    };
    let message = data
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("Gemini backend error")
        .to_string();
    AdapterError {
        message,
        response: Box::new((status, axum::Json(data)).into_response()),
        failure: None,
    }
}

/// The byte cap for a whole-body read: the call's own cap when it sets one,
/// otherwise `MAX_GEMINI_UNARY_RESPONSE_BYTES`, so a client turn (whose bounds
/// set none) is still never read into unbounded memory.
///
/// A call's own cap is never tightened: the `UpstreamBodyTooLarge` marker a
/// cap hit carries tells `routing::serve` that *its* bound was spent, which
/// would be false for an operator cap set above the adapter's ceiling.
fn unary_cap(bounds: crate::adapters::ResponseBounds) -> Option<usize> {
    Some(bounds.max_bytes.unwrap_or(MAX_GEMINI_UNARY_RESPONSE_BYTES))
}

async fn forward(
    state: AppState,
    route: Route,
    _uri: &Uri,
    _headers: &HeaderMap,
    body: RequestBody,
    bounds: crate::adapters::ResponseBounds,
) -> Result<(StatusCode, Response<Body>), AdapterError> {
    let provider = state
        .config
        .provider(&route.provider)
        .ok_or_else(|| map_gemini_error(StatusCode::INTERNAL_SERVER_ERROR, "unknown provider"))?;
    if provider.auth != AuthMode::AntigravityOauth {
        let credential = resolve_credential(&state.config, &route, &state.http_client).await?;
        return forward_single(&state, &route, body, credential, None, bounds).await;
    }
    let accounts = provider
        .resolve_pool_accounts()
        .await
        .map_err(|error| map_gemini_error(StatusCode::SERVICE_UNAVAILABLE, &error))?;
    if accounts.is_empty() {
        let credential = resolve_credential(&state.config, &route, &state.http_client).await?;
        return forward_single(&state, &route, body, credential, None, bounds).await;
    }
    if accounts.iter().all(|account| account.disabled) {
        return Err(map_gemini_error(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!(
                "provider '{}' has {} account(s) but all are `disabled = true`; none are selectable",
                route.provider,
                accounts.len()
            ),
        ));
    }
    let order = state.accounts.select_order(
        &route.provider,
        &accounts,
        None,
        Some(route.upstream_model.as_str()),
        state.config.server.pool.as_ref(),
    );
    let candidates = order.len();
    let mut last_error = None;
    for (position, index) in order.into_iter().enumerate() {
        let account = &accounts[index];
        let Some(admission) = state.accounts.admit_candidate(
            &route.provider,
            account,
            state.config.storm_ramp_initial(),
            position,
            candidates,
        ) else {
            continue;
        };
        let refresh_lock = state.accounts.refresh_lock(&route.provider, account);
        let credential = {
            let _guard = refresh_lock.lock().await;
            match crate::auth::resolve_antigravity_account(
                account,
                &state.http_client,
                &provider.base_url,
            )
            .await
            {
                Ok(credential) => credential,
                Err(error) => {
                    state.accounts.cooldown(
                        &route.provider,
                        account,
                        std::time::Duration::from_secs(5 * 60),
                        "auth",
                    );
                    tracing::warn!(
                        provider = %route.provider,
                        account = %account.name,
                        error = %error.message,
                        "failed to resolve Antigravity OAuth account"
                    );
                    continue;
                }
            }
        };
        // Captured before `credential` is moved into `forward_single`, so a
        // 401 (`FailoverAction::RefreshRetry`) has the rejected token to hand
        // `force_refresh_antigravity_account`. `resolve_antigravity_account`
        // only ever yields `AntigravityOauth`, so the `None` arm is
        // unreachable today — but this is a request-handling path in a
        // failover proxy, so degrade gracefully (log loudly, cool down and
        // rotate) instead of panicking if a future refactor ever breaks that
        // invariant.
        let rejected_access_token = match &credential {
            Credential::AntigravityOauth { access_token, .. } => Some(access_token.clone()),
            _ => {
                tracing::error!(
                    provider = %route.provider,
                    account = %account.name,
                    "antigravity_oauth account resolved a non-OAuth credential"
                );
                None
            }
        };
        match forward_single(
            &state,
            &route,
            body.clone(),
            credential,
            Some(account),
            bounds,
        )
        .await
        {
            Ok((status, mut response)) => {
                state
                    .accounts
                    .mark_healthy(&route.provider, account, status.is_success());
                state.accounts.clear_needs_relogin(&route.provider, account);
                if let Ok(value) = HeaderValue::from_str(&account.name) {
                    response.headers_mut().insert("x-shunt-account", value);
                }
                return Ok((status, crate::adapters::with_admission(response, admission)));
            }
            Err(error) => {
                // An idle gap or byte cap that cut this read is the caller's
                // bound, not this account's fault: rotating would re-spend it
                // against the next account and could replace the marker
                // `routing::serve` reads with that account's error.
                let extensions = error.response.extensions();
                if extensions
                    .get::<crate::adapters::UpstreamBodyIdle>()
                    .is_some()
                    || extensions
                        .get::<crate::adapters::UpstreamBodyTooLarge>()
                        .is_some()
                {
                    return Err(error);
                }
                let status = error.response.status();
                // The empty `HeaderMap` here only feeds the Relay-vs-not
                // decision, never the cooldown: `classify_antigravity`'s
                // status-based branches (429/401/5xx/success) never consult
                // headers, so this is provably the same decision real headers
                // would produce. The real upstream headers already informed
                // the cooldown call inside `forward_single` (Rotate/PauseSame)
                // or will inform the refresh-retry handling below
                // (RefreshRetry) — by the time an error reaches here, its
                // headers were already discarded by `map_gemini_error`.
                match crate::accounts::classify_antigravity(status, &HeaderMap::new()) {
                    crate::accounts::FailoverAction::Relay => return Err(error),
                    crate::accounts::FailoverAction::RefreshRetry => {
                        let Some(rejected_access_token) = rejected_access_token else {
                            state.accounts.cooldown(
                                &route.provider,
                                account,
                                std::time::Duration::from_secs(30),
                                "auth",
                            );
                            last_error = Some(error);
                            continue;
                        };
                        let refreshed = {
                            let _guard = refresh_lock.lock().await;
                            crate::auth::force_refresh_antigravity_account(
                                account,
                                &state.http_client,
                                &provider.base_url,
                                &rejected_access_token,
                            )
                            .await
                        };
                        match refreshed {
                            Ok(refreshed_credential) => {
                                match forward_single(
                                    &state,
                                    &route,
                                    body.clone(),
                                    refreshed_credential,
                                    Some(account),
                                    bounds,
                                )
                                .await
                                {
                                    Ok((status, mut response)) => {
                                        state.accounts.mark_healthy(
                                            &route.provider,
                                            account,
                                            status.is_success(),
                                        );
                                        state
                                            .accounts
                                            .clear_needs_relogin(&route.provider, account);
                                        if let Ok(value) = HeaderValue::from_str(&account.name) {
                                            response.headers_mut().insert("x-shunt-account", value);
                                        }
                                        return Ok((
                                            status,
                                            crate::adapters::with_admission(response, admission),
                                        ));
                                    }
                                    Err(retry_error) => {
                                        // Same as above: an idle or byte
                                        // bound is the caller's, so it ends
                                        // the pool walk rather than rotating.
                                        let extensions = retry_error.response.extensions();
                                        if extensions
                                            .get::<crate::adapters::UpstreamBodyIdle>()
                                            .is_some()
                                            || extensions
                                                .get::<crate::adapters::UpstreamBodyTooLarge>()
                                                .is_some()
                                        {
                                            return Err(retry_error);
                                        }
                                        let retry_status = retry_error.response.status();
                                        if retry_status == StatusCode::UNAUTHORIZED {
                                            // The refresh grant succeeded — the
                                            // refresh token is alive — but the
                                            // API still rejects the fresh
                                            // access token: the account is
                                            // genuinely de-authorized upstream,
                                            // not momentarily unlucky.
                                            state.accounts.cooldown(
                                                &route.provider,
                                                account,
                                                std::time::Duration::from_secs(5 * 60),
                                                "auth",
                                            );
                                            state.accounts.mark_needs_relogin(
                                                &route.provider,
                                                account,
                                                crate::accounts::ReloginCause::ServedRequest,
                                            );
                                            last_error = Some(retry_error);
                                        } else {
                                            match crate::accounts::classify_antigravity(
                                                retry_status,
                                                &HeaderMap::new(),
                                            ) {
                                                crate::accounts::FailoverAction::Relay => {
                                                    return Err(retry_error)
                                                }
                                                _ => {
                                                    // Rotate/PauseSame already
                                                    // cooled down with real
                                                    // headers inside
                                                    // forward_single.
                                                    last_error = Some(retry_error);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Err(refresh_error) => {
                                state.accounts.cooldown(
                                    &route.provider,
                                    account,
                                    std::time::Duration::from_secs(5 * 60),
                                    "auth",
                                );
                                if refresh_error.terminal {
                                    state.accounts.mark_needs_relogin(
                                        &route.provider,
                                        account,
                                        crate::accounts::ReloginCause::RefreshGrant,
                                    );
                                }
                                tracing::warn!(
                                    provider = %route.provider,
                                    account = %account.name,
                                    error = %refresh_error.error.message,
                                    terminal = refresh_error.terminal,
                                    "failed to force-refresh Antigravity OAuth account"
                                );
                                last_error = Some(refresh_error.error);
                            }
                        }
                    }
                    crate::accounts::FailoverAction::Rotate
                    | crate::accounts::FailoverAction::PauseSame => {
                        // Cooldown already applied inside forward_single using
                        // the real upstream headers (Retry-After, etc.).
                        last_error = Some(error);
                    }
                }
            }
        }
    }
    crate::metrics::record_pool_rotation(&route.provider, "exhausted");
    Err(last_error.unwrap_or_else(|| {
        map_gemini_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no Antigravity accounts are available",
        )
    }))
}

/// Cool a pooled Antigravity account down using the real upstream status and
/// headers — honoring `Retry-After` on a 429 — rather than a post-hoc,
/// headerless reclassification once the error has already been mapped for
/// the client. Skipped for `Relay` (not an account failure) and
/// `RefreshRetry` (a 401 gets its own force-refresh-then-cooldown handling in
/// `forward`, since whether it even needs a cooldown depends on that retry's
/// outcome).
fn cooldown_for_antigravity_error(
    state: &AppState,
    route: &Route,
    account: &crate::config::AccountConfig,
    status: StatusCode,
    headers: &HeaderMap,
) {
    match crate::accounts::classify_antigravity(status, headers) {
        crate::accounts::FailoverAction::Relay | crate::accounts::FailoverAction::RefreshRetry => {}
        crate::accounts::FailoverAction::Rotate | crate::accounts::FailoverAction::PauseSame => {
            let cooldown = if status == StatusCode::TOO_MANY_REQUESTS {
                crate::accounts::retry_after(headers)
                    .unwrap_or(std::time::Duration::from_secs(60))
                    .clamp(
                        std::time::Duration::from_secs(1),
                        std::time::Duration::from_secs(3600),
                    )
            } else {
                std::time::Duration::from_secs(30)
            };
            state.accounts.cooldown(
                &route.provider,
                account,
                cooldown,
                crate::accounts::rotation_reason(status, headers),
            );
        }
    }
}

async fn forward_single(
    state: &AppState,
    route: &Route,
    body: RequestBody,
    credential: Credential,
    account: Option<&crate::config::AccountConfig>,
    bounds: crate::adapters::ResponseBounds,
) -> Result<(StatusCode, Response<Body>), AdapterError> {
    let provider = state
        .config
        .provider(&route.provider)
        .ok_or_else(|| AdapterError {
            message: format!("unknown provider {}", route.provider),
            response: Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
            failure: None,
        })?;

    let (access_token, project_id) = match credential {
        Credential::GoogleOauth {
            access_token,
            project_id,
        }
        | Credential::AntigravityOauth {
            access_token,
            project_id,
        } => (access_token, project_id),
        Credential::ApiKey { value, .. } => (value, String::new()),
        _ => {
            return Err(AdapterError {
                message: "unsupported credential for Gemini adapter".to_string(),
                response: Box::new(StatusCode::UNAUTHORIZED.into_response()),
                failure: None,
            });
        }
    };

    let json_body = body.json();
    let is_streaming = json_body.get("stream").and_then(Value::as_bool) == Some(true);

    let mut inner_req = translate_request_for_model(json_body, &route.upstream_model)?;

    let base_url = provider.base_url.trim_end_matches('/');

    let method = if is_streaming {
        "streamGenerateContent?alt=sse"
    } else {
        "generateContent"
    };
    // Both subscription paths speak the Code Assist protocol: the same
    // `v1internal` methods under the `{model,project,request}` envelope. Only
    // the credential and the client identity differ, so the envelope is gated
    // on "is a subscription token", not on which provider issued it.
    let is_code_assist = matches!(
        provider.auth,
        AuthMode::GoogleOauth | AuthMode::AntigravityOauth
    );
    let (endpoint, payload) = if provider.auth == AuthMode::AntigravityOauth {
        // Antigravity speaks the same `v1internal` methods, but the client
        // identifies itself as the agent and names a session, and its catalog
        // ids carry their effort tier. See `wrap_antigravity_envelope` and
        // `antigravity_upstream_model`.
        //
        // The host is resolved rather than taken verbatim: production does not
        // serve Antigravity inference, and configs written against pre-0.40.0
        // docs pin it. Code Assist (`AuthMode::GoogleOauth`) genuinely lives
        // on production, so the redirect is scoped to this branch.
        let inference_base = inference_base_url(base_url);
        // Resolving twice is a no-op — the daily host resolves to itself — and
        // keeps the endpoint's own rule readable next to its unit tests.
        let endpoint = antigravity_endpoint(&inference_base, method);
        if !endpoint.starts_with(base_url) {
            tracing::debug!(
                provider = %route.provider,
                configured = %base_url,
                "antigravity base_url is pinned at the production Code Assist host, which does \
                 not serve inference; sending this request to the daily host instead"
            );
        }
        // The account's own catalog decides between the `-<tier>` and
        // `-tiered` forms of the same model; both exist in the wild and which
        // one an account is served changes over time. Discovery is cached and
        // fails open, so this costs at most one bounded request per account
        // per TTL and never fails the client's request. An internal call reads
        // it under its own bounds, which a cold cache would otherwise let the
        // control plane's reply escape (#637). It is skipped outright for
        // an id no catalog could reshape — only Gemini ids carry a tier — so a
        // Claude- or GPT-routed Antigravity provider never pays for it.
        let catalog = if antigravity_model_needs_catalog(&route.upstream_model) {
            catalog_ids(
                &state.http_client,
                &inference_base,
                &access_token,
                &project_id,
                bounds,
            )
            .await
        } else {
            None
        };
        let model = antigravity_upstream_model_with(
            &route.upstream_model,
            route.effort.as_deref(),
            json_body,
            catalog.as_ref().map(|catalog| AntigravityCatalog {
                ids: &catalog.ids,
                fresh: catalog.fresh,
            }),
        );
        if let Some(level) = model.thinking_level {
            set_thinking_level(&mut inner_req, level);
        }
        let session_id = antigravity_session_id(&inner_req);
        let envelope = wrap_antigravity_envelope(
            &model.id,
            &project_id,
            inner_req,
            &antigravity_request_id(),
            &session_id,
        );
        (endpoint, envelope)
    } else if is_code_assist {
        let endpoint = format!("{base_url}/v1internal:{method}");
        let envelope = wrap_code_assist_envelope(&route.upstream_model, &project_id, inner_req);
        (endpoint, envelope)
    } else {
        let model_slug = &route.upstream_model;
        let endpoint = format!("{base_url}/v1beta/models/{model_slug}:{method}");
        (endpoint, inner_req)
    };

    let policy = if account.is_some() {
        crate::retry::RetryPolicy::DISABLED
    } else {
        provider.retry.policy()
    };
    let http_client = state.http_client.clone();
    let payload_clone = payload.clone();
    let endpoint_clone = endpoint.clone();
    let is_google_oauth = is_code_assist;
    // Antigravity's backend is addressed as the Antigravity client; the Gemini
    // Code Assist path is not, so the fingerprint is per auth mode rather than
    // sent on every Code Assist request.
    let user_agent = (provider.auth == AuthMode::AntigravityOauth)
        .then(crate::auth::antigravity::version::user_agent);
    let token = access_token.clone();

    let ttfb_ms = state.config.server.timeouts.upstream_ttfb_ms;
    let retry_safety = retry_safety_for_auth(provider.auth);
    let send =
        crate::retry::send_with_retry_with_safety(policy, &route.provider, retry_safety, || {
            let client = http_client.clone();
            let payload = payload_clone.clone();
            let endpoint = endpoint_clone.clone();
            let token = token.clone();
            let user_agent = user_agent.clone();
            async move {
                let mut req = client
                    .post(&endpoint)
                    .header("Content-Type", "application/json");

                if let Some(user_agent) = user_agent {
                    req = req.header("User-Agent", user_agent);
                }

                if is_google_oauth {
                    req = req.bearer_auth(&token);
                } else {
                    req = req.header("x-goog-api-key", &token);
                }

                crate::upstream_timeout::wait(ttfb_ms, req.json(&payload).send()).await
            }
        });
    let response = match account {
        // One attempt per `send_with_retry_with_safety` run holds only because
        // a pool account's policy is `DISABLED` above; enabling pool retries
        // would count several dispatches as one.
        Some(account) => {
            send.count_attempt(&state.accounts, &route.provider, account)
                .await
        }
        None => send.await,
    }
    .map_err(|error| {
        if let Some(account) = account {
            state.accounts.cooldown(
                &route.provider,
                account,
                std::time::Duration::from_secs(30),
                "transport",
            );
        }
        error.into_adapter_error(|error| AdapterError {
            message: format!("network error calling Gemini backend: {error}"),
            response: Box::new(StatusCode::BAD_GATEWAY.into_response()),
            failure: None,
        })
    })?;

    let status = response.status();
    if !status.is_success() {
        // Captured before the body read consumes `response` — `map_gemini_error`
        // builds a fresh outbound response and carries no headers, so this is
        // the only point past which the real upstream headers exist at all.
        let response_headers = response.headers().clone();
        // Under the same bounds as a success body: an error body is no more
        // trusted than a good one, and on a bounded internal call it is read
        // into the same memory — and read before `run_chain` returns, so a
        // stall here is one `routing::serve`'s collector never sees.
        let body_text =
            match crate::adapters::collect_upstream_body(response, unary_cap(bounds), bounds.idle)
                .await
            {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(crate::adapters::UpstreamBodyError::TooLarge(too_large)) => {
                    return Err(crate::adapters::too_large_error(too_large))
                }
                // A refusal that stalls is cut exactly like a success that
                // does: the gated collector would have cut either at the same
                // gap.
                Err(crate::adapters::UpstreamBodyError::Idle(idle)) => {
                    return Err(crate::adapters::idle_error(idle))
                }
                Err(crate::adapters::UpstreamBodyError::Transport(_)) => {
                    "failed to read error response".to_string()
                }
            };
        if let Some(account) = account {
            cooldown_for_antigravity_error(state, route, account, status, &response_headers);
        }
        return Err(map_gemini_error(status, &body_text));
    }

    if is_streaming {
        let byte_stream = response.bytes_stream();
        let machine =
            GeminiSseMachine::new_streaming_for_upstream(&route.model, &route.upstream_model);
        let decoder = GeminiSseDecoder::default();

        let sse_stream = futures_util::stream::unfold(
            (
                byte_stream,
                decoder,
                machine,
                false,
                None::<Bytes>,
                None::<Bytes>,
            ),
            |(
                mut bytes,
                mut decoder,
                mut machine,
                finished,
                mut pending,
                mut deferred_terminal,
            )| async move {
                if finished {
                    return None;
                }
                loop {
                    if let Some(chunk) = pending.take() {
                        let (consumed, item) = match decoder.push_one(&chunk) {
                            Ok(step) => step,
                            Err(error) => {
                                let mut output = Vec::new();
                                append_protocol_error(error, &mut output);
                                return Some((
                                    Ok::<_, std::convert::Infallible>(Bytes::from(output)),
                                    (bytes, decoder, machine, true, None, None),
                                ));
                            }
                        };
                        if consumed < chunk.len() {
                            pending = Some(chunk.slice(consumed..));
                        }
                        let Some(item) = item else {
                            continue;
                        };

                        let is_done = item == GeminiSseItem::Done;
                        let result = match item {
                            GeminiSseItem::Json(value) => machine.process_chunk_checked(&value),
                            GeminiSseItem::Done => machine.transport_close_checked(),
                        };
                        let result_succeeded = result.is_ok();
                        let mut output = Vec::new();
                        let terminal = match result {
                            Ok(events) => {
                                let terminal = events.iter().any(|event| {
                                    event.event == "error" || event.event == "message_stop"
                                });
                                append_sse_events(events, &mut output);
                                terminal
                            }
                            Err(error) => {
                                append_protocol_error(error.to_string(), &mut output);
                                true
                            }
                        };
                        if is_done && !terminal {
                            unreachable!("[DONE] completion must be terminal");
                        }
                        if is_done && terminal && result_succeeded {
                            deferred_terminal = Some(Bytes::from(output));
                            continue;
                        }
                        if !output.is_empty() {
                            return Some((
                                Ok(Bytes::from(output)),
                                (
                                    bytes,
                                    decoder,
                                    machine,
                                    terminal,
                                    pending,
                                    deferred_terminal,
                                ),
                            ));
                        }
                        continue;
                    }

                    match bytes.next().await {
                        Some(Ok(chunk)) => {
                            pending = Some(chunk);
                        }
                        Some(Err(error)) => {
                            let mut output = Vec::new();
                            append_protocol_error(
                                format!("Gemini response stream failed: {error}"),
                                &mut output,
                            );
                            return Some((
                                Ok(Bytes::from(output)),
                                (bytes, decoder, machine, true, None, None),
                            ));
                        }
                        None => {
                            let mut output = Vec::new();
                            match decoder.finish() {
                                Err(error) => append_protocol_error(error, &mut output),
                                Ok(()) => {
                                    if let Some(terminal) = deferred_terminal {
                                        output.extend_from_slice(&terminal);
                                    } else {
                                        match machine.transport_close_checked() {
                                            Ok(events) => append_sse_events(events, &mut output),
                                            Err(error) => append_protocol_error(
                                                error.to_string(),
                                                &mut output,
                                            ),
                                        }
                                    }
                                }
                            }
                            return Some((
                                Ok(Bytes::from(output)),
                                (bytes, decoder, machine, true, None, None),
                            ));
                        }
                    }
                }
            },
        );

        let res_builder = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream; charset=utf-8")
            .header("Cache-Control", "no-cache");

        let response_res = res_builder
            .body(Body::from_stream(sse_stream))
            .map_err(|error| AdapterError {
                message: format!("failed to build response: {error}"),
                response: Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
                failure: None,
            })?;

        Ok((StatusCode::OK, response_res))
    } else {
        // Bounded for an internal call, and by `MAX_GEMINI_UNARY_RESPONSE_BYTES`
        // for a client turn: this is the non-streaming branch, so the whole
        // reply is materialised before it can be translated — and before
        // `run_chain` returns, which is why the idle gap has to bite here
        // rather than in `routing::serve`'s collector.
        let full_body =
            match crate::adapters::collect_upstream_body(response, unary_cap(bounds), bounds.idle)
                .await
            {
                Ok(bytes) => bytes,
                Err(crate::adapters::UpstreamBodyError::TooLarge(too_large)) => {
                    return Err(crate::adapters::too_large_error(too_large))
                }
                Err(crate::adapters::UpstreamBodyError::Idle(idle)) => {
                    return Err(crate::adapters::idle_error(idle))
                }
                Err(crate::adapters::UpstreamBodyError::Transport(error)) => {
                    return Err(crate::adapters::mark_body_broke(AdapterError {
                        message: format!("failed to read response body: {error}"),
                        response: Box::new(StatusCode::BAD_GATEWAY.into_response()),
                        failure: None,
                    }))
                }
            };

        let parsed = serde_json::from_slice::<Value>(&full_body).map_err(|error| {
            local_gemini_error(format!("invalid JSON from Gemini backend: {error}"))
        })?;
        let mut machine = GeminiSseMachine::new_for_upstream(&route.model, &route.upstream_model);
        let events = machine
            .process_chunk_checked(&parsed)
            .map_err(|error| local_gemini_error(error.to_string()))?;
        if let Some(error) = events.into_iter().find(|event| event.event == "error") {
            return Err(embedded_gemini_error(error.data));
        }
        machine
            .transport_close_checked()
            .map_err(|error| local_gemini_error(error.to_string()))?;
        let final_json = machine
            .final_json_checked()
            .map_err(|error| local_gemini_error(error.to_string()))?;

        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_static("application/json"));

        let response_res = (StatusCode::OK, headers, axum::Json(final_json)).into_response();
        Ok((StatusCode::OK, response_res))
    }
}

fn retry_safety_for_auth(auth: AuthMode) -> crate::retry::RetrySafety {
    if auth == AuthMode::AntigravityOauth {
        crate::retry::RetrySafety::Idempotent
    } else {
        crate::retry::RetrySafety::NonIdempotentPost
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn antigravity_retry_safety_remains_idempotent_until_phase_11() {
        assert_eq!(
            retry_safety_for_auth(AuthMode::AntigravityOauth),
            crate::retry::RetrySafety::Idempotent
        );
        assert_eq!(
            retry_safety_for_auth(AuthMode::ApiKey),
            crate::retry::RetrySafety::NonIdempotentPost
        );
    }

    #[test]
    fn unary_cap_keeps_the_call_cap_and_defaults_a_client_turn_to_the_adapter_ceiling() {
        let bounds = |max_bytes| crate::adapters::ResponseBounds {
            max_bytes,
            idle: None,
        };
        assert_eq!(
            unary_cap(bounds(None)),
            Some(MAX_GEMINI_UNARY_RESPONSE_BYTES)
        );
        assert_eq!(unary_cap(bounds(Some(1024))), Some(1024));
        // An operator cap above the adapter's ceiling stays the operator's, so a
        // cap hit's marker never claims a bound the call did not reach.
        let above = MAX_GEMINI_UNARY_RESPONSE_BYTES + 1;
        assert_eq!(unary_cap(bounds(Some(above))), Some(above));
    }

    #[test]
    fn a_production_pinned_antigravity_base_url_sends_inference_to_the_daily_host() {
        // Pre-0.40.0 docs told operators to write the production host, which
        // answers every Antigravity inference request with a fake 429. Such a
        // config still loads, so the request path is what has to redirect it.
        assert_eq!(
            antigravity_endpoint("https://cloudcode-pa.googleapis.com", "generateContent"),
            "https://daily-cloudcode-pa.googleapis.com/v1internal:generateContent"
        );
    }

    #[test]
    fn an_antigravity_base_url_in_front_of_the_backend_is_left_alone() {
        // A loopback proxy stands in front of the backend: redirecting past it
        // would egress straight around the endpoint the operator configured.
        assert_eq!(
            antigravity_endpoint("http://127.0.0.1:9999", "streamGenerateContent?alt=sse"),
            "http://127.0.0.1:9999/v1internal:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn a_thinking_level_joins_the_translated_thinking_budget_rather_than_replacing_it() {
        // The `-tiered` catalog id names no effort, so the tier rides in
        // `thinkingLevel` — beside the `thinkingBudget` the client's own
        // `thinking` block already translated to. The backend accepts both.
        let mut inner_req = serde_json::json!({
            "contents": [],
            "generationConfig": {"thinkingConfig": {"thinkingBudget": 8192}}
        });

        set_thinking_level(&mut inner_req, "medium");

        assert_eq!(
            inner_req["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "medium"
        );
        assert_eq!(
            inner_req["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            8192
        );
    }

    #[test]
    fn an_explicitly_disabled_thinking_block_is_not_overridden_by_the_default_tier() {
        // `thinking: {"type": "disabled"}` translates to `thinkingBudget: 0`,
        // and the tier the resolver derived for it is the `medium` default
        // nobody asked for. Writing the level here would send "do not think"
        // and "think at medium" in one object, and bill reasoning to a client
        // that opted out.
        let mut inner_req = serde_json::json!({
            "contents": [],
            "generationConfig": {"thinkingConfig": {"thinkingBudget": 0}}
        });

        set_thinking_level(&mut inner_req, "medium");

        assert!(
            inner_req["generationConfig"]["thinkingConfig"]
                .get("thinkingLevel")
                .is_none(),
            "{inner_req}"
        );
        assert_eq!(
            inner_req["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            0
        );
    }

    #[test]
    fn a_thinking_level_creates_the_config_objects_it_needs() {
        // A non-thinking request carries no `generationConfig` at all, so the
        // level would be dropped if the path were only ever patched in.
        let mut inner_req = serde_json::json!({"contents": []});

        set_thinking_level(&mut inner_req, "high");

        assert_eq!(
            inner_req["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
        assert!(inner_req.get("contents").is_some());
    }

    #[test]
    fn complete_utf8_line_survives_arbitrary_byte_chunking() {
        let line = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Olá 🌊\"}]}}]}\n\n";
        let split = line.find('🌊').unwrap() + 1;
        let mut decoder = GeminiSseDecoder::default();

        assert_eq!(
            decoder.push_one(&line.as_bytes()[..split]).unwrap(),
            (split, None)
        );
        let (_, Some(GeminiSseItem::Json(value))) =
            decoder.push_one(&line.as_bytes()[split..]).unwrap()
        else {
            panic!("expected JSON event")
        };
        assert_eq!(
            value.pointer("/candidates/0/content/parts/0/text"),
            Some(&Value::String("Olá 🌊".to_string()))
        );
    }

    #[test]
    fn unterminated_final_data_line_is_rejected() {
        let mut decoder = GeminiSseDecoder::default();

        assert!(decoder
            .push_one(br#"data: {"candidates":[{"finishReason":"STOP"}]}"#)
            .unwrap()
            .1
            .is_none());
        assert!(decoder.finish().unwrap_err().contains("unterminated"));
    }
}
