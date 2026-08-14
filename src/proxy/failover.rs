use std::time::Instant;

use axum::{
    body::Body,
    http::{HeaderMap, HeaderValue, StatusCode, Uri},
    response::IntoResponse,
};

use crate::{
    adapters::{
        anthropic::AnthropicAdapter, cursor::CursorAdapter, responses::ResponsesAdapter, Adapter,
        AdapterError, AdapterFailure,
    },
    config::{AuthMode, CountTokens, ProviderKind},
    count_tokens,
    error::ShuntError,
    routing::{self, AdapterKind},
    server::AppState,
};

use super::{count_tokens_unsupported, is_count_tokens, normalize_request_body, ForwardError};

pub(super) async fn forward(
    state: AppState,
    uri: &Uri,
    headers: &HeaderMap,
    body: Body,
    started_at: Instant,
) -> Result<(StatusCode, axum::response::Response), ForwardError> {
    let max_request_bytes = state.config.server.limits.max_request_bytes;
    if crate::http_tuning::content_length_exceeds(headers, max_request_bytes) {
        return Err(ForwardError {
            message: "request body exceeds the configured limit".to_string(),
            response: crate::http_tuning::request_too_large(false).await,
        });
    }
    let body = crate::http_tuning::read_body(body, max_request_bytes, false)
        .await
        .map_err(|response| ForwardError {
            message: if response.status() == StatusCode::PAYLOAD_TOO_LARGE {
                "request body exceeds the configured limit"
            } else {
                "failed to read request body"
            }
            .to_string(),
            response,
        })?;
    let mut body = crate::request::RequestBody::parse(body.to_vec())
        .map_err(routing::invalid_routing_request)
        .map_err(|error| ForwardError {
            message: "failed to route request".to_string(),
            response: error.into_response(),
        })?;
    normalize_request_body(&mut body);
    let (mut routes, requested_model) =
        routing::resolve_request_chain_value(&state.config, body.json()).map_err(|error| {
            ForwardError {
                message: "failed to route request".to_string(),
                response: error.into_response(),
            }
        })?;
    crate::observability::record_requested_model(&requested_model);
    // Records the request's final outcome exactly once, at whichever terminal
    // return point below is taken — the intermediate per-attempt failover
    // advances already have their own `tracing::warn!` + metrics and are not
    // re-recorded here (#281 asks only for the request's ultimate outcome, to
    // keep the span/event signal one-per-request rather than one-per-attempt).
    let finish = |provider: &str, status: StatusCode| {
        crate::observability::record_span_outcome(provider, status);
        crate::observability::capture_upstream_outcome(provider, &requested_model, status);
    };
    if is_count_tokens(uri) {
        // count_tokens answers from the first chain element only, so gate and
        // dispatch against just that element: a later credential-injecting
        // fallback must not force inbound auth on an otherwise-passthrough
        // count_tokens request.
        routes.truncate(1);
    }
    let (base_headers, inbound) =
        check_inbound_auth(&state, &routes, headers).map_err(|error| *error)?;
    enforce_managed_model_policy(&state, inbound.gateway_claims.as_ref(), &requested_model)
        .map_err(|error| *error)?;

    let first_route = routes
        .first()
        .expect("route chains are non-empty after resolution");
    if is_count_tokens(uri) {
        return count_tokens_response(
            state,
            first_route.clone(),
            uri,
            &base_headers,
            &inbound,
            body,
            &requested_model,
        )
        .await;
    }

    let attempted_total = routes.len();
    let mut body = Some(body);
    let last_route = routes
        .last()
        .expect("route chains are non-empty after resolution")
        .clone();
    // The caller's credential is retained across a passthrough failover only
    // when the *primary* route is itself passthrough: then the credential is the
    // caller's own upstream credential, presented for the primary's origin, so a
    // same-origin passthrough fallback may reuse it. When the primary instead
    // injects its own credential, the caller credential is a gateway/client
    // secret that must never be replayed upstream — no origin is retained and
    // every passthrough fallback strips it. Only failover attempts consult this,
    // so a single-upstream chain parses no URL here at all.
    let primary_origin = (attempted_total > 1 && is_passthrough_route(&state, first_route))
        .then(|| provider_origin(&state, &first_route.provider))
        .flatten();
    let mut remembered: Option<RememberedFailure> = None;
    for (index, route) in routes.into_iter().enumerate() {
        crate::metrics::record_failover(&route.provider, "attempted");
        let attempt_headers = headers_for_route(
            &state,
            &route,
            &base_headers,
            &inbound,
            index == 0,
            primary_origin.as_deref(),
        );
        let provider = route.provider.clone();
        let model = route.model.clone();
        let upstream_model = route.upstream_model.clone();
        let attempt_started_at = Instant::now();
        // Move the buffered body into the final attempt instead of cloning it. Within
        // this failover loop, the common single-upstream chain transfers the body
        // without copying its (up to 64 MB) raw buffer, and a multi-upstream chain
        // only clones that buffer for attempts preceding the last. Those clones
        // still share the parsed tree; an adapter may clone the body again downstream.
        // The `Option` permits the final move while keeping the body available earlier.
        let attempt_body = if index + 1 < attempted_total {
            body.as_ref().expect("request body is present").clone()
        } else {
            body.take()
                .expect("request body is present for final attempt")
        };
        let result = dispatch(state.clone(), route, uri, &attempt_headers, attempt_body).await;

        if !is_count_tokens(uri) {
            let status = match &result {
                Ok((status, _)) => status.as_u16(),
                Err(error) => error.response.status().as_u16(),
            };
            crate::metrics::record_proxied_request(
                &provider,
                &model,
                status,
                attempt_started_at.elapsed().as_secs_f64() * 1000.0,
            );
        }

        match result {
            Ok((status, mut response)) => {
                stamp_gateway_headers(&mut response, &provider, &requested_model, &upstream_model);
                if !is_advance_status(status) {
                    finish(&provider, status);
                    return Ok(observe_response(
                        status, response, provider, model, started_at,
                    ));
                }
                tracing::warn!(
                    provider = %provider,
                    model = %model,
                    status = status.as_u16(),
                    "upstream response triggered failover advance"
                );
                remember_failure(
                    &mut remembered,
                    status,
                    FinalResponse::Relayed(response),
                    provider.clone(),
                    model,
                );
            }
            Err(error) => {
                let AdapterError {
                    message,
                    mut response,
                    failure,
                } = error;
                stamp_gateway_headers(&mut response, &provider, &requested_model, &upstream_model);
                match failure {
                    Some(AdapterFailure::UpstreamStatus(raw_status))
                        if is_advance_status(raw_status) =>
                    {
                        tracing::warn!(
                            provider = %provider,
                            model = %model,
                            status = raw_status.as_u16(),
                            message = %message,
                            "upstream error triggered failover advance"
                        );
                        remember_failure(
                            &mut remembered,
                            raw_status,
                            FinalResponse::MappedError { message, response },
                            provider.clone(),
                            model,
                        );
                    }
                    Some(AdapterFailure::BeforeHeaders) => {
                        tracing::warn!(
                            provider = %provider,
                            model = %model,
                            message = %message,
                            "upstream failed before response headers; advancing failover"
                        );
                    }
                    _ => {
                        finish(&provider, response.status());
                        return Err(ForwardError {
                            message,
                            response: *response,
                        });
                    }
                }
            }
        }

        if index + 1 < attempted_total {
            crate::metrics::record_failover(&provider, "advanced");
        }
    }

    crate::metrics::record_failover(&last_route.provider, "exhausted");
    if let Some(failure) = remembered {
        return match failure.response {
            FinalResponse::Relayed(response) => {
                let status = response.status();
                finish(&failure.provider, status);
                Ok(observe_response(
                    status,
                    response,
                    failure.provider,
                    failure.model,
                    started_at,
                ))
            }
            FinalResponse::MappedError { message, response } => {
                finish(&failure.provider, response.status());
                Err(ForwardError {
                    message,
                    response: *response,
                })
            }
        };
    }

    let message = format!("all upstreams failed ({attempted_total} attempted)");
    let mut response =
        ShuntError::new(StatusCode::BAD_GATEWAY, "api_error", message.clone()).into_response();
    stamp_gateway_headers(
        &mut response,
        &last_route.provider,
        &requested_model,
        &last_route.upstream_model,
    );
    finish(&last_route.provider, StatusCode::BAD_GATEWAY);
    Err(ForwardError { message, response })
}

async fn count_tokens_response(
    state: AppState,
    route: routing::Route,
    uri: &Uri,
    base_headers: &HeaderMap,
    inbound: &InboundContext,
    body: crate::request::RequestBody,
    requested_model: &str,
) -> Result<(StatusCode, axum::response::Response), ForwardError> {
    let provider = route.provider.clone();
    let upstream_model = route.upstream_model.clone();
    let result = if matches!(
        route.adapter,
        AdapterKind::Responses
            | AdapterKind::Cursor
            | AdapterKind::Gemini
            | AdapterKind::Antigravity
    ) {
        let mode = state
            .config
            .provider(&provider)
            .map(|provider| provider.count_tokens)
            .unwrap_or(CountTokens::Estimate);
        Ok(match mode {
            CountTokens::Tiktoken => {
                // Count over the tree the inbound parse already produced instead of
                // re-deserializing the same buffer on the blocking pool. Moving only
                // the `Arc` also keeps the raw bytes out of the task: `spawn_blocking`
                // cannot be cancelled, so a client that disconnects mid-request leaves
                // this encode running to completion, and anything it captured stays
                // resident (up to a 64 MB body) for that whole window.
                let request = body.json_arc();
                let input_tokens = tokio::task::spawn_blocking(move || {
                    count_tokens::count_input_tokens_value(&request)
                })
                .await
                .unwrap_or(0);
                (
                    StatusCode::OK,
                    axum::Json(serde_json::json!({ "input_tokens": input_tokens })).into_response(),
                )
            }
            CountTokens::Estimate => count_tokens_unsupported(),
        })
    } else {
        // Single element (count_tokens uses only the first chain entry), so it is
        // the primary route — the caller's credential is kept without any origin
        // parsing.
        let headers = headers_for_route(&state, &route, base_headers, inbound, true, None);
        dispatch(state, route, uri, &headers, body).await
    };
    match result {
        Ok((status, mut response)) => {
            stamp_gateway_headers(&mut response, &provider, requested_model, &upstream_model);
            crate::observability::record_span_outcome(&provider, status);
            crate::observability::capture_upstream_outcome(&provider, requested_model, status);
            Ok((status, response))
        }
        Err(error) => {
            let mut response = *error.response;
            stamp_gateway_headers(&mut response, &provider, requested_model, &upstream_model);
            let status = response.status();
            crate::observability::record_span_outcome(&provider, status);
            crate::observability::capture_upstream_outcome(&provider, requested_model, status);
            Err(ForwardError {
                message: error.message,
                response,
            })
        }
    }
}

async fn dispatch(
    state: AppState,
    route: routing::Route,
    uri: &Uri,
    headers: &HeaderMap,
    body: crate::request::RequestBody,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    match route.adapter {
        AdapterKind::Anthropic => {
            AnthropicAdapter
                .forward(state, route, uri, headers, body)
                .await
        }
        AdapterKind::Responses => {
            ResponsesAdapter
                .forward(state, route, uri, headers, body)
                .await
        }
        AdapterKind::Cursor => {
            CursorAdapter
                .forward(state, route, uri, headers, body)
                .await
        }
        AdapterKind::Gemini => {
            crate::adapters::gemini::GeminiAdapter
                .forward(state, route, uri, headers, body)
                .await
        }
        AdapterKind::Antigravity => {
            crate::adapters::antigravity::AntigravityAdapter
                .forward(state, route, uri, headers, body)
                .await
        }
    }
}

fn observe_response(
    status: StatusCode,
    response: axum::response::Response,
    provider: String,
    model: String,
    started_at: Instant,
) -> (StatusCode, axum::response::Response) {
    let response = crate::stream_metrics::observe_response(
        response,
        crate::stream_metrics::Protocol::Anthropic,
        provider,
        model,
        started_at,
    );
    (status, response)
}

fn is_advance_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::TOO_MANY_REQUESTS
            | StatusCode::UNAUTHORIZED
            | StatusCode::FORBIDDEN
            | StatusCode::NOT_FOUND
    ) || status.is_server_error()
}

fn failure_priority(status: StatusCode) -> u8 {
    match status {
        StatusCode::TOO_MANY_REQUESTS => 4,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => 3,
        StatusCode::NOT_FOUND => 2,
        _ if status.is_server_error() => 1,
        _ => 0,
    }
}

enum FinalResponse {
    Relayed(axum::response::Response),
    MappedError {
        message: String,
        response: Box<axum::response::Response>,
    },
}

struct RememberedFailure {
    raw_status: StatusCode,
    response: FinalResponse,
    provider: String,
    model: String,
}

fn remember_failure(
    remembered: &mut Option<RememberedFailure>,
    raw_status: StatusCode,
    response: FinalResponse,
    provider: String,
    model: String,
) {
    if remembered
        .as_ref()
        .is_some_and(|current| failure_priority(current.raw_status) >= failure_priority(raw_status))
    {
        return;
    }
    *remembered = Some(RememberedFailure {
        raw_status,
        response,
        provider,
        model,
    });
}

fn enforce_managed_model_policy(
    state: &AppState,
    claims: Option<&crate::gateway::jwt::Claims>,
    requested_model: &str,
) -> Result<(), Box<ForwardError>> {
    let Some((auth, claims)) = state.gateway_auth.as_ref().zip(claims) else {
        return Ok(());
    };
    let Some(available_models) = auth
        .managed_settings(&claims.email)
        .and_then(crate::gateway::managed::available_models)
    else {
        return Ok(());
    };
    let policy_model = routing::strip_context_window_hint(requested_model);
    if available_models
        .iter()
        .any(|model| model.as_str() == Some(policy_model))
    {
        return Ok(());
    }
    let message =
        format!("model \"{requested_model}\" is not permitted by this gateway's managed policy");
    Err(Box::new(ForwardError {
        message: message.clone(),
        response: ShuntError::new(StatusCode::BAD_REQUEST, "invalid_request_error", message)
            .into_response(),
    }))
}

struct InboundContext {
    gateway_claims: Option<crate::gateway::jwt::Claims>,
    client: Option<String>,
    static_client: bool,
}

/// Authenticate once against the whole route chain. Client credential stripping
/// is deferred to [`headers_for_route`] so a passthrough attempt retains the
/// caller's upstream credential while credential-injecting attempts cannot leak
/// it. On failover, a passthrough attempt keeps the credential only while its
/// origin matches the primary upstream's, so a host-specific token is never
/// replayed to a different origin (a same-origin fallback still carries it).
fn check_inbound_auth(
    state: &AppState,
    routes: &[routing::Route],
    headers: &HeaderMap,
) -> Result<(HeaderMap, InboundContext), Box<ForwardError>> {
    let mut forwarded = headers.clone();
    forwarded.remove("x-shunt-inbound-client");
    if let Some(auth) = &state.inbound_auth {
        forwarded.remove(auth.header());
    }

    let gateway_claims = state
        .gateway_auth
        .as_ref()
        .and_then(|auth| auth.authenticate_bearer(headers));
    // One definition of "passthrough" for both the auth gate and header
    // handling. The inline form this replaced disagreed with the helper on an
    // unknown provider — it treated one as *not* credential-injecting, which
    // skipped `[server.auth]` rather than demanding it. Fail closed instead.
    let injects_credential = routes
        .iter()
        .any(|route| !is_passthrough_route(state, route));
    if !injects_credential || (state.inbound_auth.is_none() && state.gateway_auth.is_none()) {
        return Ok((
            forwarded,
            InboundContext {
                gateway_claims,
                client: None,
                static_client: false,
            },
        ));
    }

    let static_client = state
        .inbound_auth
        .as_ref()
        .and_then(|auth| auth.authenticate_client(headers));
    if static_client.is_some() || gateway_claims.is_some() {
        let client = static_client
            .map(str::to_string)
            .or_else(|| gateway_claims.as_ref().map(|claims| claims.email.clone()))
            .expect("one composed authentication branch matched");
        tracing::info!(client = %client, "inbound client authenticated for route chain");
        return Ok((
            forwarded,
            InboundContext {
                gateway_claims,
                client: Some(client),
                static_client: static_client.is_some(),
            },
        ));
    }

    tracing::warn!("inbound auth failed: missing or invalid client credential");
    let message = if let Some(auth) = &state.inbound_auth {
        format!(
            "missing or invalid credential: this gateway requires a client token (via {}, Authorization: Bearer, or x-api-key) or gateway login",
            auth.header()
        )
    } else {
        "missing or invalid credential: sign in to this gateway and send the issued bearer token"
            .to_string()
    };
    Err(Box::new(ForwardError {
        message: "inbound authentication failed".to_string(),
        response: ShuntError::new(StatusCode::UNAUTHORIZED, "authentication_error", message)
            .into_response(),
    }))
}

fn headers_for_route(
    state: &AppState,
    route: &routing::Route,
    base: &HeaderMap,
    inbound: &InboundContext,
    is_primary: bool,
    primary_origin: Option<&str>,
) -> HeaderMap {
    let injects_credential = !is_passthrough_route(state, route);
    if !injects_credential {
        // Passthrough forwards the caller's own upstream credential. That
        // credential is origin-specific to the primary upstream, so a slot is
        // kept only while the destination origin matches the primary's: a
        // failover attempt to a *different* origin strips both `authorization`
        // and `x-api-key` and fails closed rather than replay a host-specific
        // token off-origin. A same-origin failover (e.g. two passthrough
        // entries on one host) keeps a slot that holds the caller's own
        // upstream credential; the primary route is the origin the credential
        // was presented for, so it is kept without parsing any URL (the
        // single-upstream hot path).
        //
        // Within a same-origin attempt, each retained slot is also checked *by
        // value* against shunt's own gateway JWT and stripped only if it holds
        // one — never both slots just because one of them does. `authorization`
        // and `x-api-key` are the two slots an `apiKeyHelper` can fill (it
        // fills both), so shunt's own identity token can land in either or
        // both, beside a genuine upstream credential in the other slot that
        // must keep flowing. `check_inbound_auth`'s auth gate authenticates
        // once for the whole route chain, so a chain mixing mapped and
        // passthrough routes can still reach this branch same-origin with a
        // gateway JWT in one of these slots. A static `[server.auth]` token
        // can use an alternative slot in the common case — operators mixing
        // passthrough and mapped models hand out dedicated `x-shunt-token`
        // values so `authorization`/`x-api-key` stay free for the caller's
        // real credential (`docs/m4-inbound-auth.md` §2) — but that is
        // advisory, not enforced here: a static token delivered through a
        // rotating-credential mechanism like `apiKeyHelper` hits the same
        // both-slots problem, and unlike the gateway JWT this function never
        // checks a static token's value (only `discovery/upstream.rs`'s
        // `is_consumed_by_shunt` does), so it survives a same-origin
        // passthrough attempt even when it should be stripped (#357). A
        // gateway JWT has no advisory alternative and is always checked here
        // by value.
        let mut headers = base.clone();
        let same_origin = is_primary
            || matches!(
                (primary_origin, provider_origin(state, &route.provider).as_deref()),
                (Some(primary), Some(this)) if primary == this
            );
        if !same_origin {
            headers.remove("authorization");
            headers.remove("x-api-key");
            tracing::debug!(
                provider = %route.provider,
                "stripped passthrough credential slots: off_origin"
            );
            return headers;
        }
        if inbound.gateway_claims.is_some() {
            headers.remove("authorization");
            tracing::debug!(
                provider = %route.provider,
                slot = "authorization",
                "stripped passthrough credential slot: gateway_jwt"
            );
        }
        if headers
            .get("x-api-key")
            .is_some_and(|value| is_gateway_jwt(state, value))
        {
            headers.remove("x-api-key");
            tracing::debug!(
                provider = %route.provider,
                slot = "x-api-key",
                "stripped passthrough credential slot: gateway_jwt"
            );
        }
        return headers;
    }

    let mut headers = base.clone();
    headers.remove("authorization");
    headers.remove("x-api-key");
    if inbound.static_client {
        if let Some(client) = inbound
            .client
            .as_deref()
            .and_then(|value| value.parse().ok())
        {
            headers.insert("x-shunt-inbound-client", client);
        }
    }
    headers
}

/// Whether a route's provider forwards the caller's own upstream credential
/// (`AuthMode::Passthrough`) rather than injecting a gateway-held one. An
/// unknown provider is treated as credential-injecting (fail closed).
///
/// Antigravity is never passthrough, whatever its `auth` says. The exemption
/// exists because a passthrough route lends the caller nothing — their own
/// credential goes upstream, so gating it behind `[server.auth]` would protect
/// nothing. That reasoning does not survive contact with this kind: the adapter
/// ignores the caller's credential entirely and runs the operator's local `agy`
/// with `--dangerously-skip-permissions`. Since `AuthMode::Passthrough` is also
/// the default when `auth` is omitted, honouring it here would let anyone
/// reach a protected gateway and execute code as the user running shunt.
fn is_passthrough_route(state: &AppState, route: &routing::Route) -> bool {
    state
        .config
        .provider(&route.provider)
        .is_some_and(|provider| {
            provider.auth == AuthMode::Passthrough && provider.kind != ProviderKind::Antigravity
        })
}

/// The origin (scheme + host + port) of a provider's `base_url`, used to decide
/// whether a passthrough failover attempt targets the same origin the caller's
/// credential was presented for. `None` when the provider is unknown, its
/// `base_url` cannot be parsed, or the URL has an opaque origin — callers treat
/// that as "origin changed" and strip the credential (fail closed).
fn provider_origin(state: &AppState, provider: &str) -> Option<String> {
    let base_url = state.config.provider(provider)?.base_url.as_str();
    let origin = reqwest::Url::parse(base_url)
        .ok()?
        .origin()
        .ascii_serialization();
    (origin != "null").then_some(origin)
}

/// Whether `value` — the raw contents of a header slot, no `Bearer ` prefix —
/// is shunt's own gateway JWT rather than the caller's upstream credential.
/// `false` when no `[server.gateway]` auth is configured or the value is not
/// valid UTF-8: a non-UTF-8 value cannot be a JWT, so it is treated as
/// not-ours and kept.
fn is_gateway_jwt(state: &AppState, value: &HeaderValue) -> bool {
    let Some(auth) = state.gateway_auth.as_ref() else {
        return false;
    };
    let Ok(token) = value.to_str() else {
        return false;
    };
    auth.authenticate_token(token.trim()).is_some()
}

fn stamp_gateway_headers(
    response: &mut axum::response::Response,
    upstream: &str,
    model: &str,
    upstream_model: &str,
) {
    for (name, value) in [
        ("x-gateway-upstream", upstream),
        ("x-gateway-model", model),
        ("x-gateway-upstream-model", upstream_model),
    ] {
        if let Ok(value) = HeaderValue::from_str(value) {
            response.headers_mut().insert(name, value);
        }
    }
}

#[cfg(test)]
mod tests;
