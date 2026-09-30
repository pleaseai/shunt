// frame.rs
//! Inbound WebSocket control framing and bounded upstream SSE reassembly.

use axum::{extract::ws::Message, http::HeaderMap};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_CLIENT_SSE_FRAME_BYTES: usize = 4 * 1024 * 1024;

const SAFE_RESPONSE_HEADER_EXACT: &[&str] = &[
    "retry-after",
    "x-request-id",
    "openai-request-id",
    "openai-model",
    "x-models-etag",
    "x-reasoning-included",
];

/// Project upstream response metadata into the WebSocket error envelope.
///
/// Only response metadata used by Responses clients is exposed. In particular,
/// cookies, credentials, hop-by-hop headers, and Shunt-owned headers are never
/// copied to an untrusted downstream client.
pub fn safe_response_headers(headers: &HeaderMap) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    for (name, value) in headers {
        let key = name.as_str().to_ascii_lowercase();
        if SAFE_RESPONSE_HEADER_EXACT.contains(&key.as_str())
            || key.starts_with("x-ratelimit-")
            || key.starts_with("x-codex-")
        {
            if let Ok(value) = value.to_str() {
                map.insert(key, serde_json::Value::String(value.to_string()));
            }
        }
    }
    map
}

/// Build a standalone OpenAI Responses error frame for WebSocket clients.
pub fn build_ws_error_frame(
    status: u16,
    error_type: &str,
    code: &str,
    message: &str,
    headers: Option<&HeaderMap>,
) -> String {
    let safe_headers = headers.map(safe_response_headers).unwrap_or_default();
    serde_json::json!({
        "type": "error",
        "status": status,
        "error": {
            "type": error_type,
            "code": code,
            "message": message,
        },
        "headers": safe_headers
    })
    .to_string()
}

/// Inbound client frames parsed from the WebSocket connection.
#[derive(Debug, PartialEq)]
pub enum ClientFrame {
    /// `response.create` turn request.
    ResponseCreate {
        generate: bool,
        model: Option<String>,
        raw_json: serde_json::Value,
    },
    /// Acknowledgement frame; treated as a no-op (WS-03).
    ResponseProcessed,
    /// Unparseable JSON or unknown text frame types; ignored without closing (WS-03).
    IgnoredText,
    /// Binary frame; rejected as unsupported protocol error.
    BinaryUnsupported,
    /// Close frame from peer.
    Close,
}

/// Parse an incoming WebSocket message into a [`ClientFrame`].
pub fn parse_client_frame(msg: &Message) -> ClientFrame {
    match msg {
        Message::Text(text) => {
            let value: serde_json::Value = match serde_json::from_str(text) {
                Ok(v) => v,
                Err(_) => return ClientFrame::IgnoredText,
            };
            let frame_type = match value.get("type").and_then(|t| t.as_str()) {
                Some(t) => t,
                None => return ClientFrame::IgnoredText,
            };
            match frame_type {
                "response.create" => {
                    let generate = value
                        .get("generate")
                        .and_then(|g| g.as_bool())
                        .unwrap_or(true);
                    let model = value
                        .get("model")
                        .and_then(|m| m.as_str())
                        .map(ToOwned::to_owned);
                    ClientFrame::ResponseCreate {
                        generate,
                        model,
                        raw_json: value,
                    }
                }
                "response.processed" => ClientFrame::ResponseProcessed,
                _ => ClientFrame::IgnoredText,
            }
        }
        Message::Binary(_) => ClientFrame::BinaryUnsupported,
        Message::Close(_) => ClientFrame::Close,
        Message::Ping(_) | Message::Pong(_) => ClientFrame::IgnoredText,
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct WarmupResponse {
    id: &'static str,
    object: &'static str,
    created_at: u64,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    output: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct WarmupFrame {
    #[serde(rename = "type")]
    frame_type: &'static str,
    sequence_number: u64,
    response: WarmupResponse,
}

/// Build local warmup completion frames for `response.create` with `generate: false`.
///
/// Emits deterministic empty-id `response.created` (sequence 0) and
/// `response.completed` (sequence 1) frames sharing the same timestamp and empty ID.
pub fn build_warmup_completion_frames(model: Option<&str>) -> [String; 2] {
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let created = WarmupFrame {
        frame_type: "response.created",
        sequence_number: 0,
        response: WarmupResponse {
            id: "",
            object: "response",
            created_at,
            status: "in_progress",
            model: model.map(ToOwned::to_owned),
            output: Vec::new(),
        },
    };

    let completed = WarmupFrame {
        frame_type: "response.completed",
        sequence_number: 1,
        response: WarmupResponse {
            id: "",
            object: "response",
            created_at,
            status: "completed",
            model: model.map(ToOwned::to_owned),
            output: Vec::new(),
        },
    };

    [
        serde_json::to_string(&created).expect("serialize warmup created frame"),
        serde_json::to_string(&completed).expect("serialize warmup completed frame"),
    ]
}

/// Parse an SSE block string into a unified data payload.
///
/// Strips leading `data:` and optional single space, joining multiline data
/// fields with newlines. Returns `None` if no data lines exist.
pub fn parse_sse_block(block: &str) -> Option<String> {
    let mut data_lines = Vec::new();
    for line in block.split(['\r', '\n']) {
        if let Some(stripped) = line.strip_prefix("data:") {
            let val = stripped.strip_prefix(' ').unwrap_or(stripped);
            data_lines.push(val);
        }
    }
    if data_lines.is_empty() {
        None
    } else {
        Some(data_lines.join("\n"))
    }
}

/// Classify the payload's `type` field without modifying the payload.
pub fn parse_payload_type(payload: &str) -> Option<String> {
    if payload == "[DONE]" {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    value
        .get("type")
        .and_then(|t| t.as_str())
        .map(ToOwned::to_owned)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalStatus {
    Completed,
    Failed,
    Incomplete,
}

/// Classify terminal status from event type.
pub fn terminal_status_from_type(type_str: &str) -> Option<TerminalStatus> {
    match type_str {
        "response.completed" | "response.done" => Some(TerminalStatus::Completed),
        "response.failed" => Some(TerminalStatus::Failed),
        "response.incomplete" => Some(TerminalStatus::Incomplete),
        "error" => Some(TerminalStatus::Failed),
        _ => None,
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SseFrameError {
    #[error("upstream SSE frame exceeded {0} bytes")]
    TooLarge(usize),
    #[error("upstream SSE chunk exceeded {0} frame limit")]
    CountLimit(usize),
}

/// Byte-bounded SSE block framer scanning across arbitrary chunk boundaries.
pub struct BoundedSseFrameBuffer {
    max_frame_bytes: usize,
    max_frames_per_feed: usize,
    delimiter_tail: Vec<u8>,
    candidate: Vec<u8>,
    disposed: bool,
}

impl BoundedSseFrameBuffer {
    pub fn new(max_frame_bytes: usize) -> Self {
        assert!(max_frame_bytes > 0, "max_frame_bytes must be positive");
        let max_frames_per_feed = (max_frame_bytes / 1024).max(1);
        Self {
            max_frame_bytes,
            max_frames_per_feed,
            delimiter_tail: Vec::new(),
            candidate: Vec::new(),
            disposed: false,
        }
    }

    fn clear(&mut self) {
        self.delimiter_tail.clear();
        self.candidate.clear();
    }

    fn retain(&mut self, slice: &[u8]) -> Result<(), SseFrameError> {
        if slice.is_empty() {
            return Ok(());
        }
        if self.candidate.len() + slice.len() > self.max_frame_bytes {
            let max = self.max_frame_bytes;
            self.clear();
            self.disposed = true;
            return Err(SseFrameError::TooLarge(max));
        }
        self.candidate.extend_from_slice(slice);
        Ok(())
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, SseFrameError> {
        if self.disposed || chunk.is_empty() {
            return Ok(Vec::new());
        }

        let mut frames = Vec::new();
        let previous_tail = std::mem::take(&mut self.delimiter_tail);
        let tail_len = previous_tail.len();
        let total_len = tail_len + chunk.len();

        let byte_at = |index: usize| -> u8 {
            if index < tail_len {
                previous_tail[index]
            } else {
                chunk[index - tail_len]
            }
        };

        let retain_range =
            |this: &mut Self, start: usize, end: usize| -> Result<(), SseFrameError> {
                if end <= start {
                    return Ok(());
                }
                if start < tail_len {
                    let p_end = end.min(tail_len);
                    this.retain(&previous_tail[start..p_end])?;
                }
                if end > tail_len {
                    let c_start = start.saturating_sub(tail_len);
                    let c_end = end - tail_len;
                    this.retain(&chunk[c_start..c_end])?;
                }
                Ok(())
            };

        let mut index = 0;
        let mut retained_through = 0;

        let result = (|| -> Result<(), SseFrameError> {
            while index < total_len {
                let d_len = delimiter_length_at(index, total_len, &byte_at);
                match d_len {
                    None => break,
                    Some(0) => index += 1,
                    Some(len) => {
                        if frames.len() >= self.max_frames_per_feed {
                            let max_frames = self.max_frames_per_feed;
                            self.clear();
                            self.disposed = true;
                            return Err(SseFrameError::CountLimit(max_frames));
                        }
                        retain_range(self, retained_through, index)?;
                        let block = std::mem::take(&mut self.candidate);
                        frames.push(block);
                        index += len;
                        retained_through = index;
                    }
                }
            }
            retain_range(self, retained_through, index)?;
            if index < total_len {
                let mut new_tail = Vec::with_capacity(total_len - index);
                for i in index..total_len {
                    new_tail.push(byte_at(i));
                }
                self.delimiter_tail = new_tail;
            }
            Ok(())
        })();

        match result {
            Ok(()) => Ok(frames),
            Err(err) => {
                // If any parsed frame is a terminal event, do not fail retroactively.
                if frames
                    .iter()
                    .any(|block| is_responses_terminal_frame(block))
                {
                    self.clear();
                    self.disposed = true;
                    Ok(frames)
                } else {
                    Err(err)
                }
            }
        }
    }

    pub fn finish(&mut self) -> Result<Option<Vec<u8>>, SseFrameError> {
        if self.disposed {
            return Ok(None);
        }
        let tail = std::mem::take(&mut self.delimiter_tail);
        self.retain(&tail)?;
        let block = std::mem::take(&mut self.candidate);
        self.clear();
        self.disposed = true;
        if block.is_empty() {
            Ok(None)
        } else {
            Ok(Some(block))
        }
    }

    pub fn dispose(&mut self) {
        self.clear();
        self.disposed = true;
    }
}

fn delimiter_length_at<F: Fn(usize) -> u8>(
    index: usize,
    length: usize,
    byte_at: &F,
) -> Option<usize> {
    let first = byte_at(index);
    if first == b'\n' {
        if index + 1 >= length {
            return None;
        }
        let second = byte_at(index + 1);
        if second == b'\n' {
            return Some(2);
        }
        if second != b'\r' {
            return Some(0);
        }
        // LF then CR: the blank line ends in CRLF or a bare CR; the next byte
        // decides which, so wait for it rather than splitting a CRLF.
        if index + 2 >= length {
            return None;
        }
        return if byte_at(index + 2) == b'\n' {
            Some(3)
        } else {
            Some(2)
        };
    }
    if first != b'\r' {
        return Some(0);
    }
    if index + 1 >= length {
        return None;
    }
    let second = byte_at(index + 1);
    if second == b'\r' {
        return Some(2);
    }
    if second != b'\n' {
        return Some(0);
    }
    if index + 2 >= length {
        return None;
    }
    let third = byte_at(index + 2);
    if third == b'\n' {
        return Some(3);
    }
    if third != b'\r' {
        return Some(0);
    }
    // CRLF then CR: as above, the blank line ends in CRLF or a bare CR.
    if index + 3 >= length {
        return None;
    }
    if byte_at(index + 3) == b'\n' {
        Some(4)
    } else {
        Some(3)
    }
}

fn is_responses_terminal_frame(block: &[u8]) -> bool {
    let s = match std::str::from_utf8(block) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let payload = match parse_sse_block(s) {
        Some(p) => p,
        None => return false,
    };
    if let Some(t) = parse_payload_type(&payload) {
        terminal_status_from_type(&t).is_some()
    } else {
        false
    }
}

#[cfg(test)]
pub mod tests;
