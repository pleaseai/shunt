use crate::adapters::cursor::client::decode_frame_payload;
use crate::adapters::cursor::connect::ConnectFrameDecoder;
use crate::adapters::cursor::connect::{parse_connect_error, ConnectEndError, FLAG_END};
use crate::adapters::cursor::proto::AgentServerMessage;
use serde_json::Value;

/// A decoded event from the Cursor upstream response stream.
#[derive(Debug, Clone)]
pub enum CursorStreamEvent {
    Session {
        session_id: String,
    },
    ThinkingDelta {
        text: String,
    },
    TextDelta {
        text: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
    },
    End,
}

#[derive(Debug, Clone)]
pub enum CursorDecodeError {
    ConnectEnd(ConnectEndError),
    Decode(String),
}

impl CursorDecodeError {
    pub fn status(&self) -> Option<u16> {
        match self {
            CursorDecodeError::ConnectEnd(err) => Some(err.status),
            CursorDecodeError::Decode(_) => None,
        }
    }
}

impl std::fmt::Display for CursorDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CursorDecodeError::ConnectEnd(err) => write!(f, "{err}"),
            CursorDecodeError::Decode(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for CursorDecodeError {}

/// Decode upstream response bytes into a sequence of CursorStreamEvents.
///
/// Returns both the events and the final usage for the response, since the
/// upstream may send multiple update frames.
pub fn decode_upstream_response(body: &[u8]) -> Result<Vec<CursorStreamEvent>, CursorDecodeError> {
    let mut decoder = ConnectFrameDecoder::new();
    let frames = decoder
        .push(body)
        .map_err(|error| CursorDecodeError::Decode(error.to_string()))?;
    let mut events = Vec::new();

    for frame in &frames {
        if frame.flags & FLAG_END != 0 {
            // Check for Connect error in end frame
            if !frame.payload.is_empty() {
                if let Some(err) = parse_connect_error(&frame.payload) {
                    return Err(CursorDecodeError::ConnectEnd(err));
                }
            }
            events.push(CursorStreamEvent::End);
            continue;
        }

        let msg = match decode_frame_payload(frame) {
            Ok(m) => m,
            Err(_) => continue,
        };

        events.extend(events_from_message(&msg));
    }

    Ok(events)
}

/// Build an accumulated Anthropic response JSON from upstream bytes for
/// non-streaming mode.
pub fn decode_cursor_upstream(
    body: &[u8],
    message_id: &str,
    model: &str,
) -> Result<serde_json::Value, CursorDecodeError> {
    let events = decode_upstream_response(body)?;

    let mut text_content = String::new();
    let mut final_input_tokens: u64 = 0;
    let mut final_output_tokens: u64 = 0;

    for event in &events {
        match event {
            CursorStreamEvent::TextDelta { text } => text_content.push_str(text),
            CursorStreamEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            } => {
                final_input_tokens = *input_tokens;
                final_output_tokens = *output_tokens;
            }
            CursorStreamEvent::End => break,
            _ => {}
        }
    }

    // Upstream reports usage directly; when it omits input_tokens (0), fall back
    // to a minimal 1. Estimating input tokens from `text_content` would be wrong
    // — that text is the assistant's generated output, not the request prompt.
    let input_tokens = final_input_tokens.max(1);

    Ok(serde_json::json!({
        "id": message_id,
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "text", "text": text_content}
        ],
        "model": model,
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": final_output_tokens,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0
        }
    }))
}

pub(crate) fn events_from_message(msg: &AgentServerMessage) -> Vec<CursorStreamEvent> {
    let mut events = Vec::new();
    // Check for exec_server_message with session info
    if let Some(ref exec) = msg.exec_server_message {
        if let Some(ref session_id) = exec.notes_session_id {
            if !session_id.is_empty() {
                events.push(CursorStreamEvent::Session {
                    session_id: session_id.clone(),
                });
            }
        }
    }

    if let Some(ref update) = msg.interaction_update {
        // Thinking delta
        if let Some(ref td) = update.thinking_delta {
            if !td.text.is_empty() {
                events.push(CursorStreamEvent::ThinkingDelta {
                    text: td.text.clone(),
                });
            }
        }

        // Text delta
        if let Some(ref td) = update.text_delta {
            if !td.text.is_empty() {
                events.push(CursorStreamEvent::TextDelta {
                    text: td.text.clone(),
                });
            }
        }

        // Turn ended (usage + end)
        if let Some(ref te) = update.turn_ended {
            events.push(CursorStreamEvent::Usage {
                input_tokens: te.input_tokens,
                output_tokens: te.output_tokens,
                cache_read_tokens: te.cache_read_tokens,
                cache_write_tokens: te.cache_write_tokens,
            });
            events.push(CursorStreamEvent::End);
        }
    }
    events
}

/// Extract an estimate of input tokens from a MessagesRequest for usage
/// reporting. This is a rough heuristic based on JSON string length.
pub fn estimate_request_input_tokens(req: &Value) -> u64 {
    let prompt = super::request::render_cursor_prompt(req);
    (prompt.len() / 4).max(1) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::cursor::connect::encode_connect_frame;
    use crate::adapters::cursor::proto::*;
    use crate::adapters::cursor::test_frames;
    use prost::Message;

    #[test]
    fn decodes_text_and_usage_events() {
        let mut body = Vec::new();
        body.extend_from_slice(&test_frames::text_frame("Hello"));
        body.extend_from_slice(&test_frames::text_frame(" world"));
        body.extend_from_slice(&test_frames::usage_frame(10, 5));
        body.extend_from_slice(&test_frames::end_frame());

        let events = decode_upstream_response(&body).unwrap();
        assert_eq!(events.len(), 5);
        assert!(matches!(events[0], CursorStreamEvent::TextDelta { .. }));
        assert!(matches!(events[1], CursorStreamEvent::TextDelta { .. }));
        assert!(matches!(events[2], CursorStreamEvent::Usage { .. }));
        assert!(matches!(events[3], CursorStreamEvent::End));
        assert!(matches!(events[4], CursorStreamEvent::End));
    }

    #[test]
    fn decodes_thinking_delta() {
        let body = test_frames::thinking_frame("thinking...");

        let events = decode_upstream_response(&body).unwrap();
        assert_eq!(events.len(), 1);
        if let CursorStreamEvent::ThinkingDelta { text } = &events[0] {
            assert_eq!(text, "thinking...");
        } else {
            panic!("expected ThinkingDelta");
        }
    }

    #[test]
    fn decodes_session_event() {
        let msg = AgentServerMessage {
            interaction_update: None,
            exec_server_message: Some(ExecServerMessage {
                notes_session_id: Some("session-123".to_string()),
            }),
        };
        let mut payload = Vec::new();
        msg.encode(&mut payload).unwrap();
        let body = encode_connect_frame(&payload, 0).to_vec();

        let events = decode_upstream_response(&body).unwrap();
        assert_eq!(events.len(), 1);
        if let CursorStreamEvent::Session { session_id } = &events[0] {
            assert_eq!(session_id, "session-123");
        } else {
            panic!("expected Session");
        }
    }

    #[test]
    fn accumulate_response_produces_anthropic_json() {
        let mut body = Vec::new();
        body.extend_from_slice(&test_frames::text_frame("Hello world"));
        body.extend_from_slice(&test_frames::usage_frame(15, 3));
        body.extend_from_slice(&test_frames::end_frame());
        let json = decode_cursor_upstream(&body, "msg_test", "cursor-test").unwrap();
        assert_eq!(json["id"], "msg_test");
        assert_eq!(json["content"][0]["text"], "Hello world");
        assert_eq!(json["usage"]["input_tokens"].as_u64(), Some(15));
        assert_eq!(json["usage"]["output_tokens"].as_u64(), Some(3));
        assert_eq!(
            json["usage"]["cache_creation_input_tokens"].as_u64(),
            Some(0)
        );
        assert_eq!(json["usage"]["cache_read_input_tokens"].as_u64(), Some(0));
        assert_eq!(json["stop_reason"], "end_turn");
    }

    #[test]
    fn empty_upstream_produces_empty_response() {
        let json = decode_cursor_upstream(&[], "msg_empty", "cursor-test").unwrap();
        assert_eq!(json["content"][0]["text"], "");
    }

    #[test]
    fn input_tokens_are_not_inflated_by_long_output_text() {
        // A long assistant output must not drive up the reported input_tokens:
        // the upstream-reported input count is authoritative.
        let long_output = "x".repeat(4000);
        let mut body = Vec::new();
        body.extend_from_slice(&test_frames::text_frame(&long_output));
        body.extend_from_slice(&test_frames::usage_frame(12, 1000));
        body.extend_from_slice(&test_frames::end_frame());
        let json = decode_cursor_upstream(&body, "msg_tokens", "cursor-test").unwrap();
        assert_eq!(json["usage"]["input_tokens"].as_u64(), Some(12));
        assert_eq!(json["usage"]["output_tokens"].as_u64(), Some(1000));
    }

    #[test]
    fn missing_usage_falls_back_to_one_input_token() {
        // No usage frame: input_tokens falls back to a minimal 1, never 0.
        let mut body = Vec::new();
        body.extend_from_slice(&test_frames::text_frame("hi"));
        body.extend_from_slice(&test_frames::end_frame());
        let json = decode_cursor_upstream(&body, "msg_nousage", "cursor-test").unwrap();
        assert_eq!(json["usage"]["input_tokens"].as_u64(), Some(1));
        assert_eq!(json["usage"]["output_tokens"].as_u64(), Some(0));
    }

    #[test]
    fn connect_end_frame_with_error_is_rejected() {
        let json_err = serde_json::json!({
            "error": {"code": "resource_exhausted", "message": "quota exceeded"}
        });
        let payload = serde_json::to_vec(&json_err).unwrap();
        let frame = encode_connect_frame(&payload, FLAG_END);
        let result = decode_upstream_response(&frame);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.status(), Some(429));
        assert!(err.to_string().contains("quota exceeded"));
    }

    #[test]
    fn multiple_text_deltas_accumulate() {
        let mut body = Vec::new();
        body.extend_from_slice(&test_frames::text_frame("Hello "));
        body.extend_from_slice(&test_frames::text_frame("world"));
        body.extend_from_slice(&test_frames::usage_frame(10, 2));
        body.extend_from_slice(&test_frames::end_frame());

        let events = decode_upstream_response(&body).unwrap();
        let text: String = events
            .iter()
            .filter_map(|e| {
                if let CursorStreamEvent::TextDelta { text } = e {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(text, "Hello world");
    }
}
