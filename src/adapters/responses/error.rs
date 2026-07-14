//! Map upstream and gateway failures into Anthropic-shaped `AdapterError`s.
//!
//! Shared by the HTTP path (which reads a `reqwest::Response`) and the
//! websocket path (which surfaces the same fields from a failed handshake).

use axum::{http::StatusCode, response::IntoResponse};
use serde_json::{json, Value};

use crate::{adapters::AdapterError, error::ShuntError, model::responses::map_error_value};

pub(super) async fn mapped_upstream_error(
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
pub(super) fn build_upstream_error(
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

pub(super) fn own_error(message: String) -> AdapterError {
    let error = ShuntError::bad_gateway(message);
    AdapterError {
        message: "responses adapter failed".to_string(),
        response: Box::new(error.into_response()),
    }
}

/// Build the gateway-error response for a backend-sent `error` /
/// `response.failed` event captured by the machine on a non-streaming JSON path
/// (issue #113). `error` is the already-mapped Anthropic error envelope
/// ([`crate::model::responses::AnthropicSseMachine::take_backend_error`]); it
/// becomes the response body with a `502` status — SSE error events carry no upstream
/// HTTP status to preserve, and the machine mapped the envelope's `error.type`
/// against `502` to match. Emits a warning for operational visibility. The
/// streaming paths surface the same envelope inline as an SSE `error` event
/// instead. Shared by the HTTP ([`super::http::json_response`]) and websocket
/// ([`super::ws_stream::json_events_response`]) non-streaming collectors.
pub(super) fn backend_error_response(error: Value) -> axum::response::Response {
    // Borrow `error` for the log line only; the borrow ends with the macro so
    // the envelope can move into the response body below without a clone. Use the
    // `error_message` field name (not the reserved `message`, which collides with
    // the event's own format-string message), and fully-qualify `serde_json::Value`
    // — inside `warn!` a bare `Value` resolves to tracing's own `Value` trait.
    tracing::warn!(
        error_message = error
            .pointer("/error/message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("upstream request failed"),
        "responses backend sent an error event on the non-streaming JSON path"
    );
    (StatusCode::BAD_GATEWAY, axum::Json(error)).into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use serde_json::Value;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::config::AuthMode;

    use super::mapped_upstream_error;

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
}
