use prost::Message;

use crate::adapters::cursor::connect::{encode_connect_frame, ConnectFrame, FLAG_GZIP};
use crate::adapters::cursor::model::CursorModelResolution;
use crate::adapters::cursor::proto::{self, AgentClientMessage, RunRequest};
use crate::adapters::cursor::request::CursorSelectedImage;

/// HTTP client for the Cursor AgentService/Run endpoint.
pub struct CursorHttpClient {
    client: reqwest::Client,
    base_url: String,
    client_version: String,
}

impl CursorHttpClient {
    pub fn new(client: reqwest::Client, base_url: impl Into<String>) -> Self {
        Self {
            client,
            base_url: base_url.into(),
            // Cursor's backend can start rejecting stale client versions; an env
            // override lets operators bump it without a rebuild/redeploy.
            client_version: std::env::var("SHUNT_CURSOR_CLIENT_VERSION")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "0.48.5".to_string()),
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
            .header("x-cursor-client-version", &self.client_version)
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
}

impl CursorError {
    pub fn new(status: u16, message: impl Into<String>, detail: Option<String>) -> Self {
        Self {
            status,
            message: message.into(),
            detail,
            retry_after: None,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: 502,
            message: message.into(),
            detail: None,
            retry_after: None,
        }
    }

    pub fn from_reqwest(e: reqwest::Error) -> Self {
        let status = e.status().map(|s| s.as_u16()).unwrap_or(502);
        Self {
            status,
            message: e.to_string(),
            detail: None,
            retry_after: None,
        }
    }
}

impl std::fmt::Display for CursorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cursor error {}: {}", self.status, self.message)
    }
}

impl std::error::Error for CursorError {}

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
}
