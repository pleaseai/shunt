//! Inbound OpenAI Responses (Codex) endpoint (`[server.codex_endpoint]`).
//!
//! Lets the OpenAI Codex CLI point its `chatgpt_base_url` (or a custom
//! `model_provider`) at shunt and be load-balanced across a ChatGPT/Codex OAuth
//! account pool. Unlike the Anthropic Messages path (`/v1/messages`), this is a
//! **raw passthrough**: the inbound Responses body is forwarded upstream
//! unchanged and the upstream response is relayed verbatim — only the M10
//! account-pool machinery (selection, failover, refresh) is reused. See
//! `docs/m11-inbound-codex-endpoint.md`.

use std::time::Instant;

use axum::{
    body::{to_bytes, Body, Bytes},
    extract::{OriginalUri, State},
    http::{HeaderMap, Method, StatusCode},
    response::IntoResponse,
};
use serde::Deserialize;
use tracing::Instrument;

use crate::{
    adapters::{responses, AdapterError},
    compression::BodyEncoding,
    error::{ShuntError, UpstreamError},
    routing::{AdapterKind, Route},
    server::AppState,
};

/// Inbound Responses routes this handler serves, registered by
/// [`crate::server::build_router`] when `[server.codex_endpoint]` is set.
///
/// This is the single source of truth for the path set: the router registers
/// exactly these, and `concurrency::is_codex_path` classifies against them so a
/// gateway-owned error on any of them uses the OpenAI Responses envelope rather
/// than the Anthropic one (AGENTS.md). Adding a path here registers it and gives
/// it the right error shape together — they cannot drift apart.
pub(crate) const PATHS: [&str; 3] = [
    "/backend-api/codex/responses",
    "/responses",
    "/v1/responses",
];

/// Same inbound body cap as the Anthropic Messages path (`proxy::post`).
const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Minimal view of the inbound Responses body: the `model` is read only for
/// metrics/logging labels — the body itself forwards upstream byte-for-byte, so
/// a missing or malformed model never blocks the request (the upstream rejects it).
/// `model` is deserialized as a raw [`serde_json::Value`] rather than
/// `Option<String>` so [`parse_model`] can tell "field absent" apart from
/// "field present but not a string" instead of both silently becoming `None`.
#[derive(Debug, Deserialize)]
struct ModelView {
    model: Option<serde_json::Value>,
}

/// Handler for the inbound Responses routes (`/backend-api/codex/responses`,
/// `/responses`, `/v1/responses`). Mirrors `proxy::post`'s shape: snapshot the
/// live state, trace the request, and relay a gateway-owned error as a response.
pub async fn post(
    State(state): State<AppState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Body,
) -> axum::response::Response {
    let state = state.refreshed();
    let started_at = Instant::now();
    let path = uri.path().to_string();
    // The Codex CLI keys a conversation with a `session-id` header; fall back to
    // Claude Code's header for parity. Used both for the tracing span and as the
    // account-pool sticky key so one conversation stays on one account.
    let session_id = headers
        .get("session-id")
        .or_else(|| headers.get("x-claude-code-session-id"))
        .and_then(|value| value.to_str().ok())
        .filter(|session_id| !session_id.is_empty())
        .map(ToOwned::to_owned);
    // Withhold the request-derived id from exported spans unless the operator
    // opted in per backend (same rule as `proxy::post`).
    let span_session_id = if crate::telemetry::withhold_session_id() {
        ""
    } else {
        session_id.as_deref().unwrap_or("")
    };
    // See `proxy::post`'s equivalent span for why these start empty: the model
    // and outcome are only known inside `forward`, once the body is parsed and
    // the upstream has responded (`crate::observability`, #281).
    let span = tracing::info_span!(
        "codex_endpoint_request",
        method = %method,
        path = %path,
        session_id = span_session_id,
        gen_ai.request.model = tracing::field::Empty,
        shunt.provider = tracing::field::Empty,
        http.response.status_code = tracing::field::Empty,
        otel.status_code = tracing::field::Empty
    );

    async move {
        match forward(state, session_id, headers, body, started_at).await {
            Ok((status, response)) => {
                tracing::info!(
                    upstream_status = status.as_u16(),
                    latency_ms = started_at.elapsed().as_millis(),
                    "proxied inbound codex request"
                );
                response
            }
            Err(error) => {
                // Log *why* the request failed before returning the client-facing
                // response — without this a shunt-owned failure (bad credential,
                // unreachable backend, exhausted pool) leaves no server-side signal
                // an operator could grep. Mirrors `proxy::post`.
                tracing::warn!(
                    latency_ms = started_at.elapsed().as_millis(),
                    error = %error.message,
                    "inbound codex request failed"
                );
                // Gateway-owned errors on this endpoint are built with the gateway's
                // Anthropic-shaped responders (`ShuntError` / `UpstreamError` /
                // adapter+auth `AdapterError`s). A Codex CLI (or any OpenAI Responses
                // client) pointed here expects the OpenAI `{"error":{...}}` envelope,
                // so re-shape at this single boundary (status preserved). Relayed
                // upstream errors never reach here — they return verbatim as `Ok`.
                crate::error::into_openai_error_shape(error.response).await
            }
        }
    }
    .instrument(span)
    .await
}

/// A gateway-owned error from [`forward`] carrying a log message alongside the
/// client-facing response, so [`post`] can record *why* the request failed
/// (mirrors `proxy::ForwardError`). An upstream error response relayed verbatim is
/// an `Ok`, not this — only shunt-owned failures (config, auth, body read, account
/// resolution/transport) surface here.
struct ForwardError {
    message: String,
    response: axum::response::Response,
}

impl From<AdapterError> for ForwardError {
    fn from(error: AdapterError) -> Self {
        Self {
            message: error.message,
            response: *error.response,
        }
    }
}

async fn forward(
    state: AppState,
    session_id: Option<String>,
    headers: HeaderMap,
    body: Body,
    started_at: Instant,
) -> Result<(StatusCode, axum::response::Response), ForwardError> {
    // The routes are only registered when `[server.codex_endpoint]` is set, but
    // read the snapshot defensively; config validation guarantees the named
    // provider exists and uses `chatgpt_oauth`.
    let Some(codex_endpoint) = &state.config.server.codex_endpoint else {
        return Err(ForwardError {
            message: "codex endpoint is not configured".to_string(),
            response: ShuntError::bad_gateway("codex endpoint is not configured".to_string())
                .into_response(),
        });
    };
    let provider = codex_endpoint.provider.clone();

    // Inbound client auth (M4): the target provider injects a server-side Codex
    // bearer, so a configured `[server.auth]` gates this endpoint. The passthrough
    // forwards the Codex CLI's own request headers verbatim but swaps in the pool
    // account's credential and strips the shunt client-token header (in
    // `forward_codex_inbound`), so neither the client's own credential nor the
    // shunt token ever reaches the Codex backend.
    // The authenticated inbound client's name, used below to namespace the
    // account-pool sticky key. `None` when no `[server.auth]` is configured
    // (single-tenant: the bare session id keys the pool).
    let inbound_client = if let Some(auth) = &state.inbound_auth {
        // Accept the shunt token via the configured header OR an OpenAI-style
        // `Authorization: Bearer <token>` (the `OPENAI_API_KEY` / `env_key` idiom
        // the Codex CLI and llmgateway/LiteLLM setups use), so no custom header is
        // required. The client's Bearer is only checked here — it is stripped and
        // never forwarded upstream (see `forward_codex_inbound`).
        match auth.authenticate_bearer(&headers) {
            Some(client) => Some(client.to_string()),
            None => {
                tracing::warn!(
                    provider = %provider,
                    "inbound codex auth failed: missing or invalid client token"
                );
                let message = format!(
                    "missing or invalid client token for the inbound codex endpoint: provide it via the `{}` header or `Authorization: Bearer <token>` (e.g. OPENAI_API_KEY); ask the operator for one",
                    auth.header()
                );
                return Err(ForwardError {
                    message: "inbound authentication failed".to_string(),
                    response: ShuntError::new(
                        StatusCode::UNAUTHORIZED,
                        "authentication_error",
                        message,
                    )
                    .into_response(),
                });
            }
        }
    } else {
        None
    };

    let body = to_bytes(body, MAX_REQUEST_BODY_BYTES)
        .await
        .map_err(|error| {
            let message = error.to_string();
            ForwardError {
                message: message.clone(),
                response: UpstreamError::from_message(message).into_response(),
            }
        })?;

    // Read the model for metrics/logging only; the body forwards verbatim.
    let model = model_label(&headers, &body).await;
    crate::observability::record_requested_model(&model);
    // The body-`model` does not pick a provider (the endpoint is pinned to one
    // `chatgpt_oauth` provider). `request_builder` only reads `route.provider`,
    // so `model`/`upstream_model` are labels, not routing inputs.
    let route = Route {
        provider: provider.clone(),
        adapter: AdapterKind::Responses,
        model: model.clone(),
        upstream_model: model.clone(),
        effort: None,
    };

    // Namespace the account-pool sticky key with the authenticated client so that,
    // in a multi-tenant deployment, one client cannot pin another client's Codex
    // session onto a chosen pool account by replaying its `session-id` header. This
    // mirrors the outbound Responses path's `{client}:{session_id}` pool key (see
    // `adapters/responses/mod.rs`). The raw `session_id` is still what the tracing
    // span records above; only the pool key is namespaced.
    let pool_key = pool_sticky_key(inbound_client.as_deref(), session_id);

    // Pass the client's inbound headers through so the passthrough can forward the
    // Codex CLI's own request headers verbatim (swapping only the credential); the
    // shunt client-token header is stripped inside `forward_codex_inbound`.
    let result = responses::forward_codex_inbound(state, route, pool_key, headers, body).await;
    let status_code = match &result {
        Ok((status, _)) => *status,
        Err(error) => error.response.status(),
    };
    crate::observability::record_span_outcome(&provider, status_code);
    crate::observability::capture_upstream_outcome(&provider, &model, status_code);
    crate::metrics::record_proxied_request(
        &provider,
        &model,
        status_code.as_u16(),
        started_at.elapsed().as_secs_f64() * 1000.0,
    );
    result
        .map(|(status, response)| {
            let response = crate::stream_metrics::observe_response(
                response,
                crate::stream_metrics::Protocol::Responses,
                provider,
                model,
                started_at,
            );
            (status, response)
        })
        .map_err(ForwardError::from)
}

/// The label used when the request's model cannot be read (see [`model_label`]).
const UNKNOWN_MODEL: &str = "unknown";

/// Read the `model` for metrics/logging labels only — the body itself forwards
/// upstream byte-for-byte, so a body this cannot read never blocks the request
/// (the upstream rejects it).
///
/// Current Codex releases zstd-compress the Responses request body whenever both
/// of their gates pass, which includes the documented `chatgpt_base_url` client
/// shape pointed at this endpoint (issue #285). The compressed bytes relay
/// upstream fine — `content-encoding` is forwarded verbatim — but a plain
/// `from_slice` on them fails, which would silently label every metric, log line,
/// and span for the request `unknown`. So decode a zstd body for the label, and
/// log (rather than swallow) anything that still leaves the model unreadable.
///
/// [`MAX_REQUEST_BODY_BYTES`] is passed as `decode_zstd_within`'s `cap`, the same
/// absolute limit this endpoint already applies to the arrival buffer — so the
/// arrival buffer and the decoded copy can be transiently resident together, at
/// worst two buffers each up to that cap (not one, as compressing surely shrinks
/// the wire size). What actually bounds the *decode work itself* for a small,
/// hostile body is `compression::MAX_DECODE_RATIO`, not this cap: it ties worst-case
/// decoded size to a multiple of what the peer actually uploaded (issue #291).
/// A small absolute cap here instead would be unsound for the opposite reason —
/// `serde_json::from_slice` needs a *complete* document, so any truncation-style
/// cap below a real turn's size would silently relabel every large legitimate
/// turn `unknown`, regressing issue #285's fix. The ratio bound is what makes
/// keeping the large absolute cap here safe.
async fn model_label(headers: &HeaderMap, body: &Bytes) -> String {
    let decoded = match crate::compression::body_encoding(headers) {
        BodyEncoding::Identity => None,
        BodyEncoding::Zstd => {
            match crate::compression::decode_zstd_within(body.clone(), MAX_REQUEST_BODY_BYTES).await
            {
                Ok(Some(decoded)) => Some(decoded),
                Ok(None) => {
                    tracing::warn!(
                        wire_bytes = body.len(),
                        limit = MAX_REQUEST_BODY_BYTES,
                        "inbound codex body decodes past the request size limit or the \
                         compressed-to-decoded ratio bound; model label unavailable"
                    );
                    return UNKNOWN_MODEL.to_string();
                }
                Err(error) => {
                    tracing::warn!(
                        wire_bytes = body.len(),
                        error = %error,
                        "failed to decode zstd inbound codex body; model label unavailable"
                    );
                    return UNKNOWN_MODEL.to_string();
                }
            }
        }
        // A coding shunt does not decode (anything other than zstd/identity) is
        // not fatal to the label: fall through and attempt a best-effort plain
        // parse below, same as `Identity`. Returning `unknown` unconditionally
        // here would let a client suppress its own model label by sending a
        // bogus `content-encoding` header on an otherwise-plain body.
        BodyEncoding::Other => {
            tracing::warn!(
                content_encoding = ?headers.get(axum::http::header::CONTENT_ENCODING),
                "inbound codex body uses an unsupported content-encoding; \
                 attempting a best-effort plain-JSON parse for the model label"
            );
            None
        }
    };
    let parsed_body = decoded.as_deref().unwrap_or(body);
    match parse_model(parsed_body) {
        ParsedModel::Model(model) => model,
        ParsedModel::Malformed(error) => {
            tracing::warn!(
                decoded_bytes = parsed_body.len(),
                wire_bytes = body.len(),
                error = %error,
                "inbound codex body is not valid JSON; labeling metrics and logs `unknown`"
            );
            UNKNOWN_MODEL.to_string()
        }
        ParsedModel::Missing => {
            tracing::warn!(
                decoded_bytes = parsed_body.len(),
                wire_bytes = body.len(),
                "inbound codex body has no `model` field; labeling metrics and logs `unknown`"
            );
            UNKNOWN_MODEL.to_string()
        }
        ParsedModel::NotAString(value) => {
            tracing::warn!(
                decoded_bytes = parsed_body.len(),
                wire_bytes = body.len(),
                model_type = json_type_name(&value),
                "inbound codex body's `model` field is not a string; labeling metrics and logs `unknown`"
            );
            UNKNOWN_MODEL.to_string()
        }
    }
}

/// The distinguishable outcomes of reading `model` out of a decoded body, so
/// [`model_label`] can log *why* the label is unavailable instead of folding
/// malformed JSON, a missing field, and a wrong-typed field into one silent
/// `None` (as a bare `.ok().and_then(..)` chain over `Option<String>` would).
enum ParsedModel {
    Model(String),
    /// The body is not valid JSON at all.
    Malformed(serde_json::Error),
    /// Valid JSON with no `model` field (or an explicit `null`).
    Missing,
    /// Valid JSON with a `model` field that is not a string.
    NotAString(serde_json::Value),
}

fn parse_model(body: &[u8]) -> ParsedModel {
    match serde_json::from_slice::<ModelView>(body) {
        Ok(ModelView {
            model: Some(serde_json::Value::String(model)),
        }) => ParsedModel::Model(model),
        Ok(ModelView {
            model: None | Some(serde_json::Value::Null),
        }) => ParsedModel::Missing,
        Ok(ModelView { model: Some(other) }) => ParsedModel::NotAString(other),
        Err(error) => ParsedModel::Malformed(error),
    }
}

/// A short name for a JSON value's type, for the `NotAString` log line — cheaper
/// than debug-formatting the value itself and avoids echoing arbitrary client
/// input into logs.
fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Namespace the account-pool sticky key with the authenticated inbound client so
/// that, in a multi-tenant deployment, one client cannot pin another client's Codex
/// session onto a chosen pool account by replaying its `session-id` header. Mirrors
/// the outbound Responses path's `{client}:{session_id}` key (`adapters/responses/mod.rs`).
/// With no inbound auth (`client == None`) the bare session id is used — single-tenant,
/// there is no client identity to bind. Returns `None` when the request carries no
/// session id (nothing to key the pool on).
fn pool_sticky_key(client: Option<&str>, session_id: Option<String>) -> Option<String> {
    session_id.map(|session_id| match client {
        Some(client) => format!("{client}:{session_id}"),
        None => session_id,
    })
}

#[cfg(test)]
mod tests {
    use super::{model_label, pool_sticky_key, UNKNOWN_MODEL};
    use axum::{
        body::Bytes,
        http::{header::CONTENT_ENCODING, HeaderMap},
    };

    /// A body big enough that `compress_request_body` does not skip it, shaped
    /// like the real inbound Responses request (`model` first, then the turn).
    fn request_body(model: &str) -> Bytes {
        let filler = "conversation history ".repeat(200);
        Bytes::from(
            serde_json::json!({
                "model": model,
                "input": [{"role": "user", "content": filler}],
            })
            .to_string(),
        )
    }

    fn zstd_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_ENCODING, "zstd".parse().unwrap());
        headers
    }

    /// Real gzip-compressed bytes, so a fixture claiming `content-encoding: gzip`
    /// is genuinely unparseable as plain JSON rather than happening to be valid
    /// JSON that a naive fallback would accidentally read anyway.
    fn gzip_compress(body: &[u8]) -> Bytes {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(body).expect("gzip encode should succeed");
        Bytes::from(encoder.finish().expect("gzip encode should succeed"))
    }

    #[tokio::test]
    async fn reads_the_model_from_an_uncompressed_body() {
        assert_eq!(
            model_label(&HeaderMap::new(), &request_body("gpt-5.2-codex")).await,
            "gpt-5.2-codex"
        );
    }

    /// Current Codex releases zstd-compress the request body on the
    /// `chatgpt_base_url` client shape (issue #285). Before decoding, the label
    /// parse failed silently and every metric/log/span for the request was
    /// labeled `unknown`.
    #[tokio::test]
    async fn reads_the_model_from_a_zstd_body() {
        let body = crate::compression::compress_request_body(request_body("gpt-5.2-codex"))
            .await
            .expect("compression should succeed")
            .expect("the fixture should be large enough to compress");

        assert_eq!(model_label(&zstd_headers(), &body).await, "gpt-5.2-codex");
    }

    /// A body that claims `zstd` but cannot be decoded degrades to the `unknown`
    /// label — the request itself still relays verbatim.
    #[tokio::test]
    async fn falls_back_to_unknown_for_an_undecodable_zstd_body() {
        let body = request_body("gpt-5.2-codex");
        assert_eq!(model_label(&zstd_headers(), &body).await, UNKNOWN_MODEL);
    }

    /// A content coding shunt does not decode falls through to a best-effort
    /// plain parse (B5): since this fixture's bytes are genuinely
    /// gzip-compressed (not valid JSON), the parse still fails and the label
    /// still degrades to `unknown` — but for the real reason, not because
    /// `Other` was rejected unconditionally. An unconditional rejection would
    /// let a client suppress its own model label by sending a bogus
    /// `content-encoding` header on an otherwise-plain, parseable body.
    #[tokio::test]
    async fn falls_back_to_unknown_for_an_unsupported_content_encoding() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_ENCODING, "gzip".parse().unwrap());
        let body = gzip_compress(request_body("gpt-5.2-codex").as_ref());
        assert_eq!(model_label(&headers, &body).await, UNKNOWN_MODEL);
    }

    /// A body claiming an unsupported content-encoding, but that is in fact
    /// plain, parseable JSON, still yields its `model` — proving the fallback
    /// added for B5 actually reads through rather than only changing the log
    /// line.
    #[tokio::test]
    async fn reads_the_model_despite_an_unsupported_content_encoding_label() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_ENCODING, "gzip".parse().unwrap());
        assert_eq!(
            model_label(&headers, &request_body("gpt-5.2-codex")).await,
            "gpt-5.2-codex"
        );
    }

    #[tokio::test]
    async fn falls_back_to_unknown_without_a_model_field() {
        let body = Bytes::from_static(b"{\"input\":[]}");
        assert_eq!(model_label(&HeaderMap::new(), &body).await, UNKNOWN_MODEL);
    }

    /// A `model` field present but not a string (B1) degrades to `unknown` just
    /// like a missing field, rather than panicking or silently stringifying it.
    #[tokio::test]
    async fn falls_back_to_unknown_when_the_model_field_is_not_a_string() {
        let body = Bytes::from_static(b"{\"model\":42,\"input\":[]}");
        assert_eq!(model_label(&HeaderMap::new(), &body).await, UNKNOWN_MODEL);
    }

    /// Malformed JSON (not merely an unreadable `model`) degrades to `unknown`
    /// (B1) rather than propagating a parse error to the caller — the body still
    /// forwards verbatim regardless.
    #[tokio::test]
    async fn falls_back_to_unknown_for_malformed_json() {
        let body = Bytes::from_static(b"not json at all");
        assert_eq!(model_label(&HeaderMap::new(), &body).await, UNKNOWN_MODEL);
    }

    #[test]
    fn prefixes_the_authenticated_client() {
        assert_eq!(
            pool_sticky_key(Some("alice"), Some("sess-1".to_string())),
            Some("alice:sess-1".to_string())
        );
    }

    #[test]
    fn distinguishes_clients_sharing_a_session_id() {
        // Two tenants replaying the same `session-id` must not collide on the pool,
        // so one cannot pin another's session onto a chosen account.
        let alice = pool_sticky_key(Some("alice"), Some("shared".to_string()));
        let bob = pool_sticky_key(Some("bob"), Some("shared".to_string()));
        assert_ne!(alice, bob);
    }

    #[test]
    fn falls_back_to_the_bare_session_without_auth() {
        assert_eq!(
            pool_sticky_key(None, Some("sess-1".to_string())),
            Some("sess-1".to_string())
        );
    }

    #[test]
    fn is_none_without_a_session_id() {
        assert_eq!(pool_sticky_key(Some("alice"), None), None);
        assert_eq!(pool_sticky_key(None, None), None);
    }
}
