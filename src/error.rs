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

/// The wall-clock budget for turning an error response into its SSE envelope:
/// a terminal error event must never stall on a body the upstream keeps open
/// or trickles out slowly.
pub(crate) const ERROR_ENVELOPE_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// The size cap for the same read: a huge upstream error body must not be
/// buffered whole for an envelope the client only needs a summary of.
pub(crate) const ERROR_ENVELOPE_BYTES: usize = 256 * 1024;

/// Read an upstream error body under a wall-clock and size bound, decoded
/// lossily (an error summary never carries user content that must survive
/// byte-for-byte). `None` once either bound trips or the read fails.
pub(crate) async fn bounded_upstream_text(
    upstream: reqwest::Response,
    budget: std::time::Duration,
    cap: usize,
) -> Option<String> {
    let mut body = upstream;
    let mut bytes: Vec<u8> = Vec::new();
    let mut over_cap = false;
    let read = async {
        loop {
            let Some(chunk) = body.chunk().await.ok()? else {
                return if over_cap { None } else { Some(bytes) };
            };
            if over_cap {
                // Keep draining through EOF (or the budget) so reqwest can
                // reuse the keep-alive connection, retaining only the capped
                // prefix already collected.
                continue;
            }
            if bytes.len() + chunk.len() > cap {
                over_cap = true;
                continue;
            }
            bytes.extend_from_slice(&chunk);
        }
    };
    match tokio::time::timeout(budget, read).await {
        Ok(Some(bytes)) => Some(String::from_utf8_lossy(&bytes).into_owned()),
        _ => None,
    }
}

/// The JSON body of an already-built error response, for turning it into an
/// SSE `error` event envelope. The read is budgeted (wall clock and size): a
/// terminal SSE `error` event must not stall on a body the upstream keeps
/// open. A non-JSON or over-budget body falls back to a generic `api_error`
/// envelope.
pub(crate) async fn error_body_value(response: Response) -> Value {
    error_body_value_budgeted(response, ERROR_ENVELOPE_BUDGET).await
}

pub(crate) async fn error_body_value_budgeted(
    response: Response,
    budget: std::time::Duration,
) -> Value {
    match tokio::time::timeout(budget, to_bytes(response.into_body(), ERROR_ENVELOPE_BYTES)).await {
        Ok(Ok(bytes)) => serde_json::from_slice(&bytes).unwrap_or_else(|_| generic_envelope()),
        _ => generic_envelope(),
    }
}

fn generic_envelope() -> Value {
    // "upstream request failed" is the responses adapter's historical
    // fallback message (its envelopes are always pre-serialized JSON, so the
    // fallback is reachable only when a bounded read trips); the chain's
    // synthesized envelopes parse to JSON and never reach it.
    serde_json::json!({
        "type": "error",
        "error": {"type": "api_error", "message": "upstream request failed"}
    })
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

    use super::{error_body_value_budgeted, into_openai_error_shape, ShuntError, UpstreamError};

    /// A terminal SSE error envelope must never stall on an error body the
    /// upstream keeps open: the budget trips and the generic envelope stands
    /// in.
    #[tokio::test]
    async fn error_body_value_bounds_a_hanging_body() {
        use axum::body::Body;
        let hanging = Body::from_stream(futures_util::stream::pending::<
            Result<axum::body::Bytes, std::convert::Infallible>,
        >());
        let response = axum::response::Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(hanging)
            .expect("builder uses a valid status");
        let started = std::time::Instant::now();
        let envelope = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            error_body_value_budgeted(response, std::time::Duration::from_millis(50)),
        )
        .await
        .expect("the budgeted read must not wait on the hanging body");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "the budgeted read must not wait on the hanging body"
        );
        assert_eq!(envelope["error"]["type"], "api_error");
    }

    /// An over-cap body must still be drained to EOF within the budget so
    /// the keep-alive connection pools; only the capped prefix is kept. The
    /// server can only write its whole (1 MiB) body if the client keeps
    /// reading past the cap, so the byte count discriminates drain from
    /// early drop.
    #[tokio::test]
    async fn bounded_upstream_text_drains_an_over_cap_body_to_eof() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let written = Arc::new(AtomicUsize::new(0));
        let server_written = written.clone();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let n = socket.read(&mut buffer).await.expect("read");
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..n]);
            }
            let body = vec![b'x'; 1024 * 1024];
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.expect("write head");
            for chunk in body.chunks(512) {
                if socket.write_all(chunk).await.is_err() {
                    break;
                }
                server_written.fetch_add(chunk.len(), Ordering::SeqCst);
            }
            let _ = socket.shutdown().await;
        });
        let response = reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect("request");
        let result =
            super::bounded_upstream_text(response, std::time::Duration::from_secs(10), 128).await;
        assert!(result.is_none(), "an over-cap body yields the fallback");
        task.await.expect("server task");
        assert_eq!(
            written.load(Ordering::SeqCst),
            1024 * 1024,
            "the over-cap body must be drained to EOF so the connection pools"
        );
    }

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
