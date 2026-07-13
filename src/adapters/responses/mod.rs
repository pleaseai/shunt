pub mod codex_continuation;
pub mod codex_ws;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderValue, Response, StatusCode, Uri},
    response::IntoResponse,
};
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};

use self::codex_ws::{CodexWsError, CodexWsEvents};
use crate::{
    adapters::{Adapter, AdapterError, AdapterFuture},
    auth::{resolve_credential, Credential},
    config::AuthMode,
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
    let upstream_body = translate_request(&body, &route, flavor, tool_search_native)
        .map_err(|error| own_error(error.to_string()))?;
    tracing::debug!(
        provider = %route.provider,
        upstream_model = %route.upstream_model,
        upstream_request = %upstream_body,
        "responses upstream request"
    );
    let credential = resolve_credential(&state.config, &route, &state.http_client).await?;
    let auth = state
        .config
        .provider(&route.provider)
        .map(|provider| provider.auth)
        .unwrap_or_default();
    // Codex WebSocket v2 transport (issue #32), opt-in per provider and only for
    // the ChatGPT/Codex backend. HTTP stays the path for every other upstream, and
    // is the documented safety net: a websocket connect/handshake/send failure —
    // all of which happen before any event streams to the client — transparently
    // falls back to the HTTP path below, so enabling the flag can never do worse
    // than plain HTTP. (A mid-stream failure is surfaced as an Anthropic error
    // event instead; by then the response has already begun.)
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
        session_id.as_deref(),
    )
    .await
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
    session_id: Option<&str>,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let upstream = request_builder(state, route, credential, session_id)
        .body(upstream_body.to_string())
        .send()
        .await
        .map_err(|error| own_error(error.to_string()))?;
    let status = upstream.status();
    if !status.is_success() {
        return Err(mapped_upstream_error(status, upstream, auth).await);
    }
    if client_wants_stream {
        let keepalive = std::time::Duration::from_secs(state.config.server.sse_keepalive_seconds);
        Ok((
            StatusCode::OK,
            stream_response(
                upstream,
                route.model.clone(),
                thinking_enabled,
                tool_search_native,
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
    keepalive: std::time::Duration,
) -> axum::response::Response {
    let bytes = upstream.bytes_stream();
    let parser = SseParser::default();
    let machine = AnthropicSseMachine::new(model, thinking_enabled, tool_search_native);
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
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let pool_key = pool_key.filter(|key| !key.is_empty());
    let http_url = responses_url(&state.config, &route.provider);
    let ws_url = codex_ws::to_websocket_url(&http_url).map_err(ws_transport_error)?;
    let ctx = WsTurnContext {
        ws_url,
        pool_key,
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

    let (buffered, events) = open_ws_turn(&ctx).await?;
    if client_wants_stream {
        let keepalive = std::time::Duration::from_secs(state.config.server.sse_keepalive_seconds);
        Ok((
            StatusCode::OK,
            stream_events_response(
                buffered,
                events,
                route.model.clone(),
                thinking_enabled,
                tool_search_native,
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
    credential: Credential,
    auth: AuthMode,
    signature: String,
    full_input: Vec<Value>,
    upstream_body: Value,
}

/// A buffered first event, peeked to catch a rejected `previous_response_id`.
type BufferedEvent = Option<Result<ResponseEvent, CodexWsError>>;

/// Open a websocket turn, applying `previous_response_id` continuation when the
/// connection is reused and the input is an append-only extension. If the backend
/// rejects the replayed id, retry once with the full input on a fresh connection.
/// Returns a peeked first event (only when continuation was used) plus the rest of
/// the stream.
async fn open_ws_turn(
    ctx: &WsTurnContext<'_>,
) -> Result<(BufferedEvent, CodexWsEvents), AdapterError> {
    let (mut events, used_continuation) = start_ws_turn(ctx, true).await?;
    if !used_continuation {
        return Ok((None, events));
    }
    // Peek the first event: a rejected previous_response_id arrives before any
    // output, so we can transparently retry with the full input.
    match events.recv().await {
        Some(Err(error)) if error.previous_response_missing => {
            tracing::info!("codex previous_response_id rejected; retrying with full input");
            let (events, _) = start_ws_turn(ctx, false).await?;
            Ok((None, events))
        }
        buffered => Ok((buffered, events)),
    }
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
        if let Some(stored) = turn.stored_continuation() {
            if let Some(decision) = codex_continuation::decide(&stored, &ctx.upstream_body) {
                if let Some(object) = frame_body.as_object_mut() {
                    object.insert("input".to_string(), json!(decision.input_delta));
                    object.insert(
                        "previous_response_id".to_string(),
                        json!(decision.previous_response_id),
                    );
                    if let Some(turn_state) = stored
                        .turn_state
                        .clone()
                        .or_else(|| turn.handshake_turn_state().map(str::to_string))
                    {
                        let metadata = object
                            .entry("client_metadata".to_string())
                            .or_insert_with(|| json!({}));
                        if let Some(metadata) = metadata.as_object_mut() {
                            metadata.insert("x-codex-turn-state".to_string(), json!(turn_state));
                        }
                    }
                }
                used_continuation = true;
                tracing::debug!(
                    delta_items = decision.input_delta.len(),
                    "codex websocket continuing with previous_response_id"
                );
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
/// [`stream_response`] but reads from the websocket event channel; a mid-stream
/// transport error is surfaced as an Anthropic `error` event so the client sees a
/// reason rather than a silent truncation. `buffered` is the peeked first event,
/// if any, replayed before the rest of the channel.
fn stream_events_response(
    buffered: BufferedEvent,
    events: CodexWsEvents,
    model: String,
    thinking_enabled: bool,
    tool_search_native: bool,
    keepalive: std::time::Duration,
) -> axum::response::Response {
    let machine = AnthropicSseMachine::new(model, thinking_enabled, tool_search_native);
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
/// non-streaming client. A mid-stream transport error returns a gateway error
/// instead of presenting partial output as a successful response. `buffered` is
/// the peeked first event, if any.
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
                None => json!({"message": crate::auth::xai_auth::refresh_error_message(status)}),
            }
        } else {
            serde_json::from_str(&text).unwrap_or_else(|_| json!({"message": text}))
        };
    let xai_tier_gate =
        status == StatusCode::FORBIDDEN && auth == crate::config::AuthMode::XaiOauth;
    let shunt_status = if status == StatusCode::UNAUTHORIZED
        || status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::BAD_REQUEST
        || xai_tier_gate
    {
        status
    } else {
        StatusCode::BAD_GATEWAY
    };
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
    use serde_json::Value;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::{
        auth::Credential,
        config::{AuthMode, Config},
        routing::{AdapterKind, Route},
        server::AppState,
    };

    use super::{build_test_request, mapped_upstream_error, responses_url};

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
    async fn maps_403_to_bad_gateway_for_other_auth_modes() {
        let upstream = upstream_response(403, "forbidden", &[]).await;
        let error = mapped_upstream_error(StatusCode::FORBIDDEN, upstream, AuthMode::ApiKey).await;
        assert_eq!(error.response.status(), StatusCode::BAD_GATEWAY);
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
    async fn remaps_5xx_to_bad_gateway_but_passes_429_through() {
        let upstream = upstream_response(503, "service unavailable", &[]).await;
        let error =
            mapped_upstream_error(StatusCode::SERVICE_UNAVAILABLE, upstream, AuthMode::ApiKey)
                .await;
        assert_eq!(error.response.status(), StatusCode::BAD_GATEWAY);

        let upstream = upstream_response(429, "{}", &[]).await;
        let error =
            mapped_upstream_error(StatusCode::TOO_MANY_REQUESTS, upstream, AuthMode::ApiKey).await;
        assert_eq!(error.response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn preserves_retry_after_header_on_429() {
        let upstream = upstream_response(429, "{}", &[("retry-after", "7")]).await;
        let error =
            mapped_upstream_error(StatusCode::TOO_MANY_REQUESTS, upstream, AuthMode::ApiKey).await;
        assert_eq!(error.response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error.response.headers().get("retry-after").unwrap(), "7");
    }
}
