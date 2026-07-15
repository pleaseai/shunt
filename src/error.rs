use axum::{
    body::to_bytes,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug)]
pub struct UpstreamError {
    message: String,
}

impl UpstreamError {
    pub fn from_reqwest(error: reqwest::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }

    pub fn from_message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, Serialize)]
struct AnthropicErrorBody {
    #[serde(rename = "type")]
    kind: &'static str,
    error: AnthropicErrorDetail,
}

#[derive(Debug, Serialize)]
struct AnthropicErrorDetail {
    #[serde(rename = "type")]
    kind: &'static str,
    message: String,
}

impl IntoResponse for UpstreamError {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_GATEWAY,
            Json(AnthropicErrorBody {
                kind: "error",
                error: AnthropicErrorDetail {
                    kind: "api_error",
                    message: self.message,
                },
            }),
        )
            .into_response()
    }
}

#[derive(Debug)]
pub struct ShuntError {
    status: StatusCode,
    kind: &'static str,
    message: String,
}

impl ShuntError {
    pub fn new(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
        }
    }

    pub fn bad_gateway(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "api_error", message)
    }
}

impl IntoResponse for ShuntError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(AnthropicErrorBody {
                kind: "error",
                error: AnthropicErrorDetail {
                    kind: self.kind,
                    message: self.message,
                },
            }),
        )
            .into_response()
    }
}

/// OpenAI Responses-shaped error body: `{"error":{"message":..,"type":..,"code":null}}`.
/// Used only by the inbound Codex endpoint (`[server.codex_endpoint]`), whose
/// clients speak the OpenAI Responses protocol and expect this envelope rather
/// than the Anthropic `{"type":"error",...}` shape the gateway uses everywhere else.
#[derive(Debug, Serialize)]
struct OpenAiErrorBody {
    error: OpenAiErrorDetail,
}

#[derive(Debug, Serialize)]
struct OpenAiErrorDetail {
    message: String,
    #[serde(rename = "type")]
    kind: String,
    /// Always `null` for gateway-owned errors — serialized (not skipped) so the
    /// body matches the shape an OpenAI Responses client parses.
    code: Option<String>,
}

/// Re-shape a gateway-owned, Anthropic-shaped error [`Response`] into the OpenAI
/// Responses error envelope, preserving the HTTP status.
///
/// The inbound Codex endpoint reuses the gateway's Anthropic-shaped responders
/// ([`ShuntError`], [`UpstreamError`], and the adapter/auth `AdapterError`s), but a
/// Codex CLI — or any OpenAI Responses client — pointed at it expects
/// `{"error":{...}}` instead, so its own error path can surface a meaningful
/// message rather than a raw/garbled one. Relayed *upstream* errors never reach
/// this: the passthrough returns them verbatim as `Ok`, so only shunt-owned
/// failures are re-shaped here. The status code (and thus the client's retry
/// behavior) is unchanged.
pub async fn into_openai_error_shape(response: Response) -> Response {
    let status = response.status();
    // Gateway-owned error bodies are tiny JSON envelopes; cap the read at 64 KiB
    // as defense-in-depth. The bound is never hit in practice, and an oversized
    // body degrades to the empty-message fallback below rather than an OOM.
    let body = to_bytes(response.into_body(), 64 * 1024).await.ok();
    let (kind, message) = body
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
        .and_then(|value| {
            let detail = value.get("error")?;
            let message = detail.get("message").and_then(Value::as_str)?.to_string();
            // Preserve the Anthropic `error.type` (e.g. `authentication_error`,
            // `api_error`) so the OpenAI-shaped `type` still carries the same
            // gateway semantics; default defensively if it is ever absent.
            let kind = detail
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("api_error")
                .to_string();
            Some((kind, message))
        })
        .unwrap_or_else(|| {
            // The gateway-owned responders always emit the Anthropic envelope, so
            // this only guards an unexpected body: keep the status and surface
            // whatever text there was rather than an empty error.
            let message = body
                .as_deref()
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                .unwrap_or_default();
            ("api_error".to_string(), message)
        });
    (
        status,
        Json(OpenAiErrorBody {
            error: OpenAiErrorDetail {
                message,
                kind,
                code: None,
            },
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use serde_json::Value;

    use super::{into_openai_error_shape, ShuntError, UpstreamError};

    async fn body_json(response: axum::response::Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable");
        serde_json::from_slice(&bytes).expect("error body should be JSON")
    }

    #[tokio::test]
    async fn reshapes_shunt_error_401_to_openai_shape() {
        // A gateway-owned auth failure keeps its 401 status but is re-wrapped in
        // the OpenAI `{"error":{message,type,code}}` envelope, preserving the type.
        let response = ShuntError::new(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "missing client token",
        )
        .into_response();
        let reshaped = into_openai_error_shape(response).await;
        assert_eq!(reshaped.status(), StatusCode::UNAUTHORIZED);
        let body = body_json(reshaped).await;
        // OpenAI shape: no top-level `type: "error"`, and the detail is under `error`.
        assert!(body.get("type").is_none());
        assert_eq!(body["error"]["message"], "missing client token");
        assert_eq!(body["error"]["type"], "authentication_error");
        assert!(body["error"].get("code").is_some_and(Value::is_null));
    }

    #[tokio::test]
    async fn reshapes_upstream_error_502_to_openai_shape() {
        let response =
            UpstreamError::from_message("all Codex OAuth accounts failed").into_response();
        let reshaped = into_openai_error_shape(response).await;
        assert_eq!(reshaped.status(), StatusCode::BAD_GATEWAY);
        let body = body_json(reshaped).await;
        // No top-level Anthropic `type:"error"` — else an unchanged envelope would pass.
        assert!(body.get("type").is_none());
        assert_eq!(body["error"]["message"], "all Codex OAuth accounts failed");
        assert_eq!(body["error"]["type"], "api_error");
        assert!(body["error"].get("code").is_some_and(Value::is_null));
    }

    #[tokio::test]
    async fn falls_back_when_body_is_not_the_anthropic_envelope() {
        // A non-Anthropic body must still yield a valid OpenAI error (status kept,
        // raw text surfaced) rather than an empty or panicking response.
        let response = (StatusCode::BAD_GATEWAY, "plain text boom").into_response();
        let reshaped = into_openai_error_shape(response).await;
        assert_eq!(reshaped.status(), StatusCode::BAD_GATEWAY);
        let body = body_json(reshaped).await;
        assert_eq!(body["error"]["message"], "plain text boom");
        assert_eq!(body["error"]["type"], "api_error");
        assert!(body["error"].get("code").is_some_and(Value::is_null));
    }
}
