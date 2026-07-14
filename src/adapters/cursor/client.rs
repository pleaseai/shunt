use prost::Message;

use crate::adapters::cursor::connect::{encode_connect_frame, ConnectFrame, FLAG_GZIP};
use crate::adapters::cursor::model::CursorModelResolution;
use crate::adapters::cursor::proto::{self, AgentClientMessage, RunRequest};
use crate::adapters::cursor::request::CursorSelectedImage;

/// Resolve the Cursor client version once, process-wide. `CursorHttpClient::new`
/// runs per request and `std::env::var` takes a global lock, so cache the lookup
/// in a `OnceLock` (the version is deploy-time config, not per-request).
fn cursor_client_version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION
        .get_or_init(|| {
            std::env::var("SHUNT_CURSOR_CLIENT_VERSION")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "0.48.5".to_string())
        })
        .as_str()
}

/// HTTP client for the Cursor AgentService/Run endpoint.
pub struct CursorHttpClient {
    client: reqwest::Client,
    base_url: String,
    client_version: &'static str,
}

impl CursorHttpClient {
    pub fn new(client: reqwest::Client, base_url: impl Into<String>) -> Self {
        Self {
            client,
            base_url: base_url.into(),
            // Cursor's backend can start rejecting stale client versions; an env
            // override lets operators bump it without a rebuild/redeploy. Resolve
            // it once process-wide (this constructor runs per request, and
            // std::env::var takes a global lock), caching in a OnceLock.
            client_version: cursor_client_version(),
        }
    }

    pub async fn run_agent(
        &self,
        token: &str,
        prompt: &str,
        resolved: &CursorModelResolution,
        images: &[CursorSelectedImage],
    ) -> Result<reqwest::Response, CursorError> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let run_request = build_run_request(prompt, resolved, images, &request_id);
        let msg = AgentClientMessage {
            run_request: Some(run_request),
            client_heartbeat: None,
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload)
            .map_err(|error| CursorError::internal(format!("prost encode: {error}")))?;
        let body = encode_connect_frame(&payload, 0);
        let url = format!(
            "{}/agent.v1.AgentService/Run",
            self.base_url.trim_end_matches('/')
        );
        self.client
            .post(url)
            .bearer_auth(token)
            .header("content-type", "application/connect+proto")
            .header("connect-protocol-version", "1")
            .header("connect-accept-encoding", "gzip")
            .header("x-cursor-client-type", "cli")
            .header("x-cursor-client-version", self.client_version)
            .header("x-ghost-mode", "true")
            .header("x-request-id", &request_id)
            .header("x-original-request-id", &request_id)
            .header("x-cursor-streaming", "true")
            .header("te", "trailers")
            .body(body)
            .send()
            .await
            .map_err(CursorError::from_reqwest)
    }
}

fn build_run_request(
    prompt: &str,
    resolved: &CursorModelResolution,
    images: &[CursorSelectedImage],
    request_id: &str,
) -> RunRequest {
    let selected_images: Vec<proto::SelectedImage> = images
        .iter()
        .map(|img| proto::SelectedImage {
            data: img.data.clone(),
            uuid: img.uuid.clone(),
            path: img.path.clone(),
            mime_type: img.mime_type.clone(),
        })
        .collect();

    RunRequest {
        conversation_state: Some(proto::ConversationState {
            messages: Vec::new(),
        }),
        action: Some(proto::Action {
            user_message_action: Some(proto::UserMessageAction {
                user_message: Some(proto::UserMessage {
                    text: prompt.to_string(),
                    message_id: request_id.to_string(),
                    selected_context: if selected_images.is_empty() {
                        None
                    } else {
                        Some(proto::SelectedContext { selected_images })
                    },
                    mode: resolved.mode.as_str().to_string(),
                }),
            }),
        }),
        mcp_tools: None,
        conversation_id: String::new(),
        requested_model: Some(proto::CursorModel {
            model_id: resolved.model_id.clone(),
            parameters: Vec::new(),
        }),
        exclude_workspace_context: false,
        selected_subagent_models: vec![],
        conversation_group_id: String::new(),
        client_supports_inline_images: true,
    }
}

/// Decode a single Connect frame payload into an AgentServerMessage.
/// Handles gzip decompression if the FLAG_GZIP bit is set.
pub fn decode_frame_payload(
    frame: &ConnectFrame,
) -> Result<proto::AgentServerMessage, CursorError> {
    // Only gzip frames need an owned, decompressed buffer; uncompressed frames
    // are decoded directly from the borrowed slice to avoid a per-frame copy.
    let payload: std::borrow::Cow<[u8]> = if frame.flags & FLAG_GZIP != 0 {
        std::borrow::Cow::Owned(
            super::connect::decode_gzip_frame(&frame.payload)
                .map_err(|e| CursorError::internal(format!("gzip decompress: {e}")))?,
        )
    } else {
        std::borrow::Cow::Borrowed(&frame.payload[..])
    };

    proto::AgentServerMessage::decode(&payload[..])
        .map_err(|e| CursorError::internal(format!("prost decode: {e}")))
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CursorError {
    pub status: u16,
    pub message: String,
    pub detail: Option<String>,
    pub retry_after: Option<String>,
    /// Whether this is a transient transport failure safe to retry. Set only by
    /// [`CursorError::from_reqwest`] for a connection-level error (connect
    /// refused/reset, timeout); a deterministic local error ([`internal`]) or a
    /// structured error carrying an explicit status ([`new`]) is never transient.
    /// Captured here instead of being re-derived from the 502-defaulted `status`,
    /// so the retry classification stays aligned with the `reqwest::Error` impl.
    ///
    /// [`internal`]: CursorError::internal
    /// [`new`]: CursorError::new
    transient: bool,
}

impl CursorError {
    pub fn new(status: u16, message: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            status,
            message: message.into(),
            detail,
            retry_after: None,
            transient: false,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: 502,
            message: message.into(),
            detail: None,
            retry_after: None,
            transient: false,
        }
    }

    pub fn from_reqwest(e: reqwest::Error) -> Self {
        // Only a connection-level failure (connect refused/reset, timeout) is a
        // transient blip worth retrying; a builder/redirect/decode error is
        // deterministic and left alone — mirroring the `reqwest::Error`
        // RetryableError impl so both adapters agree on what "transient" means.
        let transient = e.is_connect() || e.is_timeout();
        let status = e.status().map(|s| s.as_u16()).unwrap_or(502);
        Self {
            status,
            message: e.to_string(),
            detail: None,
            retry_after: None,
            transient,
        }
    }
}

impl std::fmt::Display for CursorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cursor error {}: {}", self.status, self.message)
    }
}

impl std::error::Error for CursorError {}

impl crate::retry::RetryableError for CursorError {
    /// Retry only a genuine connection-level transport failure — the `transient`
    /// flag captured at construction, set solely by [`CursorError::from_reqwest`]
    /// for a connect/timeout error. A deterministic local error
    /// ([`CursorError::internal`], e.g. a prost encode failure) or a structured
    /// error carrying an explicit status ([`CursorError::new`]) is left alone,
    /// matching the `reqwest::Error` impl the Anthropic/Responses adapters use.
    /// Genuine upstream 429/5xx arrive as an `Ok` `reqwest::Response` (reqwest does
    /// not error on HTTP status), so they are classified from the response, never
    /// here; Cursor's decoded stream errors take a separate path
    /// ([`CursorDecodeError`](super::response::CursorDecodeError)) that never
    /// reaches this trait at all.
    fn is_transient(&self) -> bool {
        self.transient
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::cursor::connect::{ConnectFrameDecoder, FLAG_GZIP};
    use crate::adapters::cursor::model::resolve_cursor_model;
    use crate::adapters::cursor::test_frames;

    fn image(uuid: &str) -> CursorSelectedImage {
        CursorSelectedImage {
            data: "base64data".to_string(),
            uuid: uuid.to_string(),
            path: "claude-image-1.png".to_string(),
            mime_type: "image/png".to_string(),
        }
    }

    #[test]
    fn build_run_request_maps_images_into_selected_context() {
        let resolved = resolve_cursor_model("cursor").unwrap();
        let images = [image("img-1")];
        let request = build_run_request("hello", &resolved, &images, "req-1");

        let user = request
            .action
            .unwrap()
            .user_message_action
            .unwrap()
            .user_message
            .unwrap();
        assert_eq!(user.text, "hello");
        assert_eq!(user.message_id, "req-1");
        let context = user.selected_context.expect("images populate context");
        assert_eq!(context.selected_images.len(), 1);
        assert_eq!(context.selected_images[0].uuid, "img-1");
        assert_eq!(context.selected_images[0].mime_type, "image/png");
        assert_eq!(request.requested_model.unwrap().model_id, resolved.model_id);
        assert!(request.client_supports_inline_images);
    }

    #[test]
    fn build_run_request_without_images_has_no_context() {
        let resolved = resolve_cursor_model("cursor").unwrap();
        let request = build_run_request("hi", &resolved, &[], "req-2");
        let user = request
            .action
            .unwrap()
            .user_message_action
            .unwrap()
            .user_message
            .unwrap();
        assert!(user.selected_context.is_none());
    }

    #[test]
    fn decode_frame_payload_decodes_plain_frame() {
        // A plain (non-gzip) text frame round-trips through the decoder.
        let bytes = test_frames::text_frame("hello");
        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(bytes).unwrap();
        let message = decode_frame_payload(&frames[0]).unwrap();
        assert!(message.interaction_update.is_some());
    }

    #[test]
    fn decode_frame_payload_rejects_malformed_payload() {
        // field 1, wire type 2 (length-delimited), length 0xFF but no data.
        let bytes = crate::adapters::cursor::connect::encode_connect_frame([0x0A, 0xFF], 0);
        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(bytes).unwrap();
        let error = decode_frame_payload(&frames[0]).unwrap_err();
        assert_eq!(error.status, 502);
    }

    #[test]
    fn decode_frame_payload_handles_gzip_frame() {
        // A gzip-flagged frame is decompressed before decoding.
        let plain = test_frames::text_frame("gzipped");
        let mut decoder = ConnectFrameDecoder::new();
        let frame = decoder.push(plain).unwrap().remove(0);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, &frame.payload).unwrap();
        let compressed = gz.finish().unwrap();
        let gzip_bytes =
            crate::adapters::cursor::connect::encode_connect_frame(&compressed, FLAG_GZIP);
        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(gzip_bytes).unwrap();
        let message = decode_frame_payload(&frames[0]).unwrap();
        assert!(message.interaction_update.is_some());
    }

    #[tokio::test]
    async fn run_agent_posts_to_agent_service() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/agent.v1.AgentService/Run"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(Vec::new()))
            .mount(&server)
            .await;

        let client = CursorHttpClient::new(reqwest::Client::new(), server.uri());
        let resolved = resolve_cursor_model("cursor").unwrap();
        let response = client
            .run_agent("token", "prompt", &resolved, &[image("i")])
            .await
            .unwrap();
        assert!(response.status().is_success());
    }

    #[tokio::test]
    async fn run_agent_transient_status_is_retried_by_the_shared_driver() {
        use std::time::Duration;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // The Cursor adapter (`cursor/mod.rs::forward`) wraps `run_agent` in
        // `send_with_retry` with the provider's policy — the same wiring the
        // Anthropic/Responses paths each get an integration test for. Drive that
        // exact combination here: a transient 503 must re-issue `run_agent` up to
        // the retry budget (1 initial + 2 retries = 3 upstream hits) and then
        // surface the last response, so a regression that dropped Cursor's retry
        // (e.g. a policy lookup silently falling back to DISABLED) would fail here.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/agent.v1.AgentService/Run"))
            .respond_with(ResponseTemplate::new(503).set_body_bytes(Vec::new()))
            .expect(3)
            .mount(&server)
            .await;

        let client = CursorHttpClient::new(reqwest::Client::new(), server.uri());
        let resolved = resolve_cursor_model("cursor").unwrap();
        let policy = crate::retry::RetryPolicy {
            max_retries: 2,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
            multiplier: 2.0,
        };
        let response = crate::retry::send_with_retry(policy, "cursor", || {
            client.run_agent("token", "prompt", &resolved, &[])
        })
        .await
        .expect("a transient HTTP status returns Ok(response), never Err");
        // The mock's `.expect(3)` (verified on drop) proves the retry fired; the
        // surfaced status is the last transient response.
        assert_eq!(response.status().as_u16(), 503);
    }

    #[tokio::test]
    async fn from_reqwest_connect_error_is_classified_transient() {
        // A real connection-refused error (port 1) is a connect failure, so the
        // retry layer must treat the resulting CursorError as transient — the
        // behavior issue #48 requires for "connection errors".
        let error = reqwest::Client::new()
            .post("http://127.0.0.1:1/agent.v1.AgentService/Run")
            .body(Vec::new())
            .send()
            .await
            .expect_err("connecting to port 1 must fail");
        assert!(error.is_connect());
        let cursor_error = CursorError::from_reqwest(error);
        assert!(
            crate::retry::RetryableError::is_transient(&cursor_error),
            "a connect-level failure must be retryable"
        );
    }

    #[test]
    fn internal_error_is_not_transient() {
        // A deterministic local failure (e.g. a prost encode error) maps to a
        // 502 status but must NOT be retried — an identical retry can't fix it.
        let error = CursorError::internal("prost encode: boom");
        assert_eq!(error.status, 502);
        assert!(!crate::retry::RetryableError::is_transient(&error));
    }

    #[test]
    fn decoded_upstream_error_is_not_transient() {
        // A status decoded from an upstream connect-error frame arrives after the
        // response and is surfaced, not retried.
        let error = CursorError::new(503, "upstream unavailable", None);
        assert!(!crate::retry::RetryableError::is_transient(&error));
    }
}
