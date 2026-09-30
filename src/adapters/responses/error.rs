//! Map upstream and gateway failures into Anthropic-shaped `AdapterError`s.
//!
//! Shared by the HTTP path (which reads a `reqwest::Response`) and the
//! websocket path (which surfaces the same fields from a failed handshake).

use std::convert::Infallible;

use axum::{
    body::{Body, Bytes},
    http::{Response, StatusCode},
    response::IntoResponse,
};
use serde_json::{json, Value};

use crate::{adapters::AdapterError, error::ShuntError, model::responses::map_error_value};

pub(super) async fn mapped_upstream_error(
    status: StatusCode,
    upstream: reqwest::Response,
    auth: crate::config::AuthMode,
) -> AdapterError {
    mapped_upstream_error_within(status, upstream, auth, None).await
}

/// [`mapped_upstream_error`] for an attempt a gated call's idle clock bounds:
/// `clock` is the gap and the instant it started, the attempt's send (see
/// [`crate::adapters::within_idle`]), so the wait for the error body is the
/// rest of the gap the header wait began rather than a fresh one (#704). The
/// body is then read here, before the error is returned, so a stall is the
/// call's cut — the [`crate::adapters::UpstreamBodyIdle`] marker — rather than
/// a lazy read left to the envelope budget alone.
///
/// `None` is every client turn: the body stays lazy, read only when the
/// chosen response is consumed, exactly as it always was.
pub(super) async fn mapped_upstream_error_within(
    status: StatusCode,
    upstream: reqwest::Response,
    auth: crate::config::AuthMode,
    clock: Option<(std::time::Duration, tokio::time::Instant)>,
) -> AdapterError {
    let retry_after = upstream.headers().get("retry-after").cloned();
    let shunt_status = crate::model::responses::client_facing_status(status);
    let body = match clock {
        None => Body::from_stream(futures_util::stream::once(async move {
            let text = crate::error::bounded_upstream_text(
                upstream,
                crate::error::ERROR_ENVELOPE_BUDGET,
                crate::error::ERROR_ENVELOPE_BYTES,
            )
            .await;
            Ok::<Bytes, Infallible>(mapped_error_body(status, text, auth))
        })),
        Some(clock) => match crate::error::bounded_upstream_text_within(
            upstream,
            crate::error::ERROR_ENVELOPE_BUDGET,
            crate::error::ERROR_ENVELOPE_BYTES,
            clock,
        )
        .await
        {
            Ok(text) => Body::from(mapped_error_body(status, text, auth)),
            Err(idle) => return crate::adapters::idle_error(idle),
        },
    };
    let mut response = Response::builder()
        .status(shunt_status)
        .header("content-type", "application/json")
        .body(body)
        .expect("valid mapped upstream error response");
    if let Some(retry_after) = retry_after {
        response.headers_mut().insert("retry-after", retry_after);
    }
    AdapterError {
        message: format!("upstream responses request failed with {status}"),
        response: Box::new(response),
        failure: Some(crate::adapters::AdapterFailure::UpstreamStatus(status)),
    }
}

/// The Anthropic-shaped envelope [`mapped_upstream_error_within`] answers with,
/// from the upstream error body's text (`None` once a read bound tripped).
fn mapped_error_body(
    status: StatusCode,
    text: Option<String>,
    auth: crate::config::AuthMode,
) -> Bytes {
    // A budget trip or read failure must not leave the envelope with an
    // empty message: name the status instead, matching the anthropic
    // fallback.
    let text = text.unwrap_or_else(|| format!("upstream returned {status}"));
    tracing::warn!(%status, ?auth, upstream_error_body = %text, "responses upstream error");
    let value = upstream_error_value(status, &text, auth);
    Bytes::from(serde_json::to_vec(&map_error_value(&value, status)).unwrap_or_default())
}

fn upstream_error_value(status: StatusCode, text: &str, auth: crate::config::AuthMode) -> Value {
    if status == StatusCode::UNAUTHORIZED && auth == crate::config::AuthMode::ChatgptOauth {
        json!({"message": "ChatGPT authentication failed; run codex login"})
    } else if status == StatusCode::UNAUTHORIZED && auth == crate::config::AuthMode::XaiOauth {
        json!({"message": "xAI authentication failed; run shunt login xai"})
    } else if status == StatusCode::FORBIDDEN && auth == crate::config::AuthMode::XaiOauth {
        let hint = "if this is the xAI subscription tier gate, re-logging in will not help — set XAI_API_KEY or upgrade your plan";
        let upstream_message = serde_json::from_str::<Value>(text)
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
        serde_json::from_str(text).unwrap_or_else(|_| json!({"message": text}))
    }
}

/// Re-shape a buffered websocket handshake failure into an Anthropic-shaped
/// [`AdapterError`]. HTTP Responses errors use [`mapped_upstream_error`] so their
/// body remains lazy until the chosen response is consumed.
pub(super) fn build_upstream_error(
    status: StatusCode,
    retry_after: Option<String>,
    text: String,
    auth: crate::config::AuthMode,
) -> AdapterError {
    tracing::warn!(%status, ?auth, upstream_error_body = %text, "responses upstream error");
    let value = upstream_error_value(status, &text, auth);
    let shunt_status = crate::model::responses::client_facing_status(status);
    let mut response = (shunt_status, axum::Json(map_error_value(&value, status))).into_response();
    if let Some(retry_after) = retry_after.and_then(|value| value.parse().ok()) {
        response.headers_mut().insert("retry-after", retry_after);
    }
    AdapterError {
        message: format!("upstream responses request failed with {status}"),
        response: Box::new(response),
        failure: Some(crate::adapters::AdapterFailure::UpstreamStatus(status)),
    }
}

pub(super) fn transport_error(message: String) -> AdapterError {
    let mut error = own_error(message);
    error.failure = Some(crate::adapters::AdapterFailure::BeforeHeaders);
    error
}

pub(super) fn own_error(message: String) -> AdapterError {
    let error = ShuntError::bad_gateway(message);
    AdapterError {
        message: "responses adapter failed".to_string(),
        response: Box::new(error.into_response()),
        failure: None,
    }
}

/// Build the gateway error for a backend-sent `error` / `response.failed` event
/// captured by the machine on a non-streaming JSON path (issue #113). `error` is
/// the already-mapped Anthropic error envelope and `status` the client-facing
/// status it was mapped against
/// ([`crate::model::responses::AnthropicSseMachine::take_backend_error`]):
/// `502` by default — SSE error events carry no upstream HTTP status to
/// preserve — or the status the error `code` classifies to
/// ([`crate::model::responses::backend_error_status`]: `429` for a throttle,
/// `529` for an overload, `400` for a terminal policy refusal), or the `status`
/// a Codex websocket wrapped error frame carries, so the client sees the same
/// envelope that HTTP status produces. Either way
/// the error is terminal (`failure: None`): the event arrived after the upstream
/// accepted the turn with 2xx headers, and a post-acceptance failure is never
/// replayed on the next upstream (`docs/upstreams-failover.md` §3 — the turn is
/// not idempotent). Emits a warning for operational visibility. The streaming
/// paths surface the same envelope inline as an SSE `error` event instead.
/// Shared by the HTTP
/// ([`super::http::json_response`]) and websocket
/// ([`super::ws_stream::json_events_response`]) non-streaming collectors.
pub(super) fn backend_error(status: StatusCode, error: Value) -> AdapterError {
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
    AdapterError {
        message: "responses backend error event".into(),
        response: Box::new((status, axum::Json(error)).into_response()),
        failure: None,
    }
}

/// Extract the already-mapped Anthropic error envelope from an
/// [`AdapterError`] so the streaming transports can re-emit it as one SSE
/// `error` event after the early `message_start` has already committed the
/// response. Every error this module builds serializes the envelope as its
/// JSON body, so the body bytes ARE the envelope. The extraction is
/// [`crate::error::error_body_value`] — the same response-body-to-envelope
/// conversion, kept in one place.
pub(super) async fn adapter_error_envelope(error: AdapterError) -> Value {
    crate::error::error_body_value(*error.response).await
}

#[cfg(test)]
pub(super) mod tests {
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use serde_json::Value;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::config::AuthMode;

    use super::{
        adapter_error_envelope, mapped_upstream_error, mapped_upstream_error_within, Bytes,
    };

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

    #[tokio::test]
    async fn adapter_error_envelope_preserves_the_mapped_body() {
        use crate::adapters::AdapterError;
        let error = crate::error::ShuntError::bad_gateway("upstream timed out");
        let adapter = AdapterError {
            message: "responses adapter failed".into(),
            response: Box::new(error.into_response()),
            failure: None,
        };
        let envelope = adapter_error_envelope(adapter).await;
        assert_eq!(envelope["type"], "error");
        assert_eq!(envelope["error"]["type"], "api_error");
        assert_eq!(envelope["error"]["message"], "upstream timed out");
    }

    /// A `400` built in-process, as a gated attempt's upstream answers it:
    /// `body` is sent whole at its absolute instant and then ends, and `None`
    /// is a body that never sends a byte.
    pub(in crate::adapters::responses) fn timed_bad_request(
        body: Option<(tokio::time::Instant, &'static [u8])>,
    ) -> reqwest::Response {
        let chunk = futures_util::stream::once(async move {
            let Some((at, bytes)) = body else {
                return futures_util::future::pending().await;
            };
            tokio::time::sleep_until(at).await;
            Ok::<_, std::io::Error>(Bytes::from_static(bytes))
        });
        reqwest::Response::from(
            axum::http::Response::builder()
                .status(400)
                .header("content-type", "application/json")
                .body(reqwest::Body::wrap_stream(chunk))
                .unwrap(),
        )
    }

    /// A gated attempt's `400` whose headers arrive 250 ms after the send and
    /// whose body then stalls is cut at the idle gap measured from the send,
    /// 300 ms, with the idle marker, rather than left to the envelope budget
    /// (#704).
    ///
    /// Non-vacuity: map it with no clock (the lazy client read) and no marker
    /// is set; start the first deadline at the read instead of at the send and
    /// the cut lands at 550 ms. Either way this goes red.
    #[tokio::test(start_paused = true)]
    async fn a_gated_error_body_stalled_after_its_headers_is_cut_at_the_gap_from_the_send() {
        let idle = std::time::Duration::from_millis(300);
        let sent_at = tokio::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let error = mapped_upstream_error_within(
            StatusCode::BAD_REQUEST,
            timed_bad_request(None),
            AuthMode::ApiKey,
            Some((idle, sent_at)),
        )
        .await;
        assert_eq!(
            error
                .response
                .extensions()
                .get::<crate::adapters::UpstreamBodyIdle>(),
            Some(&crate::adapters::UpstreamBodyIdle { idle }),
            "got: {}",
            error.message
        );
        assert!(
            error.failure.is_none(),
            "the cut is the call's, not the route's"
        );
        assert_eq!(sent_at.elapsed(), idle, "cut at the gap from the send");
    }

    /// The twin: the same `400` whose body completes 280 ms after the send is
    /// mapped as it always is — the upstream's status, message, and failure.
    #[tokio::test(start_paused = true)]
    async fn a_gated_error_body_inside_the_gap_from_the_send_is_mapped_as_before() {
        let idle = std::time::Duration::from_millis(300);
        let sent_at = tokio::time::Instant::now();
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let body = Some((
            sent_at + std::time::Duration::from_millis(280),
            &br#"{"error":{"message":"bad field"}}"#[..],
        ));
        let error = mapped_upstream_error_within(
            StatusCode::BAD_REQUEST,
            timed_bad_request(body),
            AuthMode::ApiKey,
            Some((idle, sent_at)),
        )
        .await;
        assert_eq!(error.response.status(), StatusCode::BAD_REQUEST);
        assert!(matches!(
            error.failure,
            Some(crate::adapters::AdapterFailure::UpstreamStatus(
                StatusCode::BAD_REQUEST
            ))
        ));
        assert_eq!(body_json(error).await["error"]["message"], "bad field");
    }
}
