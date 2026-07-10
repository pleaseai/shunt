use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Response, StatusCode, Uri},
    response::IntoResponse,
};
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};

use crate::{
    adapters::{Adapter, AdapterError, AdapterFuture},
    auth::{resolve_credential, Credential},
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
        _headers: &'a HeaderMap,
        body: Vec<u8>,
    ) -> AdapterFuture<'a> {
        Box::pin(async move { forward(state, route, body).await })
    }
}

async fn forward(
    state: AppState,
    route: Route,
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
    let upstream_body =
        translate_request(&body, &route, flavor).map_err(|error| own_error(error.to_string()))?;
    tracing::debug!(
        provider = %route.provider,
        upstream_model = %route.upstream_model,
        upstream_request = %upstream_body,
        "responses upstream request"
    );
    let credential = resolve_credential(&state.config, &route, &state.http_client).await?;
    let upstream = request_builder(&state, &route, credential)
        .body(upstream_body.to_string())
        .send()
        .await
        .map_err(|error| own_error(error.to_string()))?;
    let status = upstream.status();
    if !status.is_success() {
        let auth = state
            .config
            .provider(&route.provider)
            .map(|provider| provider.auth)
            .unwrap_or_default();
        return Err(mapped_upstream_error(status, upstream, auth).await);
    }
    if client_wants_stream {
        let keepalive = std::time::Duration::from_secs(state.config.server.sse_keepalive_seconds);
        Ok((
            StatusCode::OK,
            stream_response(upstream, route.model, thinking_enabled, keepalive),
        ))
    } else {
        Ok((
            StatusCode::OK,
            json_response(upstream, route.model, thinking_enabled).await?,
        ))
    }
}

fn stream_response(
    upstream: reqwest::Response,
    model: String,
    thinking_enabled: bool,
    keepalive: std::time::Duration,
) -> axum::response::Response {
    let bytes = upstream.bytes_stream();
    let parser = SseParser::default();
    let machine = AnthropicSseMachine::new(model, thinking_enabled);
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
) -> Result<axum::response::Response, AdapterError> {
    let body = upstream
        .text()
        .await
        .map_err(|error| own_error(error.to_string()))?;
    let mut machine = AnthropicSseMachine::new(model, thinking_enabled);
    for event in parse_sse_events(&body) {
        let _ = machine.apply(event);
    }
    Ok((StatusCode::OK, axum::Json(machine.final_json())).into_response())
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

fn request_builder(
    state: &AppState,
    route: &Route,
    credential: Credential,
) -> reqwest::RequestBuilder {
    let mut request = state
        .http_client
        .post(responses_url(&state.config, &route.provider))
        .header("content-type", "application/json");
    // `OpenAI-Beta: responses=experimental` is an OpenAI/ChatGPT header; xAI's
    // Responses API doesn't expect it and the reference clients don't send it.
    if state.config.responses_flavor(&route.provider) != crate::config::ResponsesFlavor::Xai {
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
        }
        // xAI subscription OAuth: bearer only, no account-id/originator headers.
        Credential::XaiOauth { access_token } => {
            request = request.bearer_auth(access_token);
        }
        // A Responses provider configured with passthrough auth is a
        // misconfiguration; send no credential and let the upstream reject it.
        Credential::Passthrough => {}
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
) -> reqwest::Request {
    request_builder(state, route, credential)
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
        let state = AppState {
            config: Config::default(),
            http_client: reqwest::Client::new(),
            inbound_auth: None,
        };

        let request = build_test_request(
            &state,
            &codex_route(),
            Credential::ChatGptOAuth {
                access_token: "access-token".to_string(),
                account_id: "account-id".to_string(),
            },
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

    #[test]
    fn builds_xai_request_bearer_only_without_openai_beta() {
        let state = AppState {
            config: Config::default(),
            http_client: reqwest::Client::new(),
            inbound_auth: None,
        };

        let request = build_test_request(
            &state,
            &xai_route(),
            Credential::XaiOauth {
                access_token: "xai-access".to_string(),
            },
        );

        // Stock /responses path on the xAI base URL.
        assert_eq!(request.url().as_str(), "https://api.x.ai/v1/responses");
        assert_eq!(
            request.headers().get("authorization").unwrap(),
            "Bearer xai-access"
        );
        // No ChatGPT/Codex headers and no OpenAI-Beta for the xai flavor.
        assert!(request.headers().get("chatgpt-account-id").is_none());
        assert!(request.headers().get("originator").is_none());
        assert!(request.headers().get("user-agent").is_none());
        assert!(request.headers().get("version").is_none());
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
