//! Current Cursor **agent** transport implementing `agent.v1.AgentService/Run`.
//!
//! Cursor moved the CLI/agent path off the old `api2.cursor.sh` host (which now
//! rejects it with HTTP 464 / `invalid x-api-key`, issue #170). The transport the
//! current `cursor-agent` CLI uses is a *paced, bidirectional* Connect-over-HTTP/2
//! stream against `agentn.global.api5.cursor.sh/agent.v1.AgentService/Run`:
//!
//! The empty HTTP 464 is an AWS ALB protocol-mismatch status (`Server:
//! awselb/2.0`): the agent path only serves HTTP/2, so an HTTP/1.1 request is
//! rejected before Cursor sees it. This transport therefore requires the shared
//! `reqwest` client to negotiate h2 via ALPN, which needs reqwest's `http2`
//! feature (enabled in `Cargo.toml`).
//!
//! * Connect framing: `[1 flag byte][4-byte BE len][payload]`; flag `0x01` = gzip
//!   payload, `0x02` = end-of-stream trailer (JSON, `{}` on success or
//!   `{"error":…}`).
//! * The logical `RunInput` is split across several request frames (frame 0 =
//!   `RunRequest`, frame 1 = environment context, then small marker frames).
//! * The client keeps the request stream **open** while reading the response,
//!   emitting periodic heartbeats and pacing the marker frames; half-closing
//!   before the server streams yields `internal: No exec result`, so the pacing
//!   is load-bearing.
//! * Assistant answer text arrives as `f1.f1.f1` string chunks and reasoning as
//!   `f1.f4.f1`.
//!
//! Wire shape reverse-engineered from the public MIT-licensed `1jehuang/jcode`
//! project (`crates/jcode-provider-cursor-runtime/src/agent_transport.rs`), which
//! captured it from the real `cursor-agent` CLI. This is a text-only transport:
//! tool bridging and image context are not yet mapped to the new wire format.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{interval_at, Instant};

use crate::adapters::cursor::client::CursorError;
use crate::adapters::cursor::connect::{
    decode_gzip_frame, parse_connect_error, ConnectFrameDecoder, FLAG_END, FLAG_GZIP,
};
use crate::adapters::cursor::response::CursorStreamEvent;

/// Default agent host. Cursor serves the CLI/agent `AgentService/Run` path here,
/// not on the old `api2.cursor.sh` (issue #170).
const AGENT_BASE_URL: &str = "https://agentn.global.api5.cursor.sh";
const AGENT_PATH: &str = "/agent.v1.AgentService/Run";
/// Client version advertised to Cursor's agent service. Must track a currently
/// served `cursor-agent` CLI build; Cursor raises the accepted-version floor and
/// rejects stale clients (the old `0.48.x` IDE version now 464s). Override at
/// runtime with `SHUNT_CURSOR_CLIENT_VERSION` when Cursor moves the floor.
const CLI_CLIENT_VERSION: &str = "cli-2026.07.08-0c04a8a";

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// Generation can take a few seconds to start; allow a long first-byte budget.
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(60);
/// Cursor keeps the response side open after the assistant message when it
/// expects a tool exec-result (which this text-only transport never sends), so
/// finish the turn once output goes quiet.
const IDLE_TIMEOUT: Duration = Duration::from_secs(4);

/// Resolve the agent base URL once, process-wide. Falls back to the default when
/// the override is empty or points off a Cursor host (never leak the
/// subscription bearer off-origin).
fn agent_base_url() -> &'static str {
    static BASE: OnceLock<String> = OnceLock::new();
    BASE.get_or_init(|| {
        let Some(raw) = std::env::var("SHUNT_CURSOR_AGENT_BASE_URL")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        else {
            return AGENT_BASE_URL.to_string();
        };
        match reqwest::Url::parse(&raw) {
            Ok(url)
                if url.host_str().is_some_and(crate::config::host_is_cursor)
                    && url.scheme() == "https" =>
            {
                raw
            }
            _ => {
                tracing::warn!(
                    "ignoring SHUNT_CURSOR_AGENT_BASE_URL {raw:?}: not an https cursor.sh host; \
                     using {AGENT_BASE_URL}"
                );
                AGENT_BASE_URL.to_string()
            }
        }
    })
    .as_str()
}

/// Resolve the advertised client version once, process-wide.
fn client_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION
        .get_or_init(|| {
            std::env::var("SHUNT_CURSOR_CLIENT_VERSION")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| CLI_CLIENT_VERSION.to_string())
        })
        .as_str()
}

/// HTTP/2 client for the current Cursor agent transport.
pub struct CursorAgentClient {
    client: reqwest::Client,
    base_url: &'static str,
    client_version: &'static str,
}

impl CursorAgentClient {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            base_url: agent_base_url(),
            client_version: client_version(),
        }
    }

    /// Open a single agent turn: start the paced request stream and return once
    /// the response headers arrive. The returned turn keeps the request stream
    /// open (with heartbeats) until it is read to completion or dropped.
    pub async fn open_turn(
        &self,
        token: &str,
        prompt: &str,
        model_id: &str,
        cwd: &str,
    ) -> Result<CursorAgentTurn, CursorError> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let frames = build_run_frames(prompt, model_id, cwd);

        // The request body is fed by a paced sender task so the server sees the
        // marker frames arrive over time (single-shot half-close yields
        // `No exec result`). A bounded channel provides backpressure; dropping
        // the sender (on stop or completion) ends the body and half-closes.
        let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        let sender = tokio::spawn(async move {
            for (idx, frame) in frames.into_iter().enumerate() {
                if tx.send(Ok(frame)).await.is_err() {
                    return;
                }
                // Frames 0 (RunRequest) and 1 (context) need the most settle time
                // before the marker frames follow.
                let pace = match idx {
                    0 => Duration::from_millis(1500),
                    1 => Duration::from_millis(800),
                    _ => Duration::from_millis(400),
                };
                tokio::select! {
                    _ = &mut stop_rx => return,
                    _ = tokio::time::sleep(pace) => {}
                }
            }
            let mut ticker = interval_at(Instant::now() + HEARTBEAT_INTERVAL, HEARTBEAT_INTERVAL);
            loop {
                tokio::select! {
                    _ = &mut stop_rx => return,
                    _ = ticker.tick() => {
                        if tx.send(Ok(heartbeat_frame())).await.is_err() {
                            return;
                        }
                    }
                }
            }
        });

        let body =
            reqwest::Body::wrap_stream(futures_util::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            }));

        let url = format!("{}{}", self.base_url.trim_end_matches('/'), AGENT_PATH);
        let response = self
            .client
            .post(url)
            .bearer_auth(token)
            .header("connect-accept-encoding", "gzip,br")
            .header("connect-protocol-version", "1")
            .header("content-type", "application/connect+proto")
            .header("user-agent", "connect-es/1.6.1")
            .header("x-cursor-client-type", "cli")
            .header("x-cursor-client-version", self.client_version)
            .header("x-ghost-mode", "true")
            .header("x-request-id", &request_id)
            .header("x-original-request-id", &request_id)
            .body(body)
            .send()
            .await
            .map_err(CursorError::from_reqwest)?;

        Ok(CursorAgentTurn {
            response,
            guard: TurnGuard {
                _stop: stop_tx,
                _sender: sender,
            },
        })
    }
}

/// Keeps the paced request stream alive for the lifetime of a turn. Dropping it
/// signals the sender task to stop and half-close the request body.
struct TurnGuard {
    _stop: oneshot::Sender<()>,
    _sender: JoinHandle<()>,
}

/// An open agent turn: response headers received, request stream still open.
pub struct CursorAgentTurn {
    response: reqwest::Response,
    guard: TurnGuard,
}

impl CursorAgentTurn {
    pub fn status(&self) -> reqwest::StatusCode {
        self.response.status()
    }

    /// Take the raw response for error mapping. Drops the guard, stopping the
    /// request stream (an error turn produces no assistant output).
    pub fn into_response(self) -> reqwest::Response {
        self.response
    }

    /// Stream decoded assistant events, applying idle/first-byte timeouts to end
    /// the turn once the server goes quiet. The guard is held until the stream
    /// completes so heartbeats keep flowing while the response is read.
    pub fn into_event_stream(self) -> impl Stream<Item = Result<CursorStreamEvent, CursorError>> {
        let state = ReadState {
            bytes: self.response.bytes_stream().boxed(),
            decoder: ConnectFrameDecoder::new(),
            pending: VecDeque::new(),
            _guard: self.guard,
            got_text: false,
            finished: false,
        };
        futures_util::stream::unfold(state, |mut state| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((event, state));
                }
                if state.finished {
                    return None;
                }
                let budget = if state.got_text {
                    IDLE_TIMEOUT
                } else {
                    FIRST_BYTE_TIMEOUT
                };
                match tokio::time::timeout(budget, state.bytes.next()).await {
                    Ok(Some(Ok(chunk))) => state.ingest(&chunk),
                    Ok(Some(Err(error))) => {
                        state.finished = true;
                        state
                            .pending
                            .push_back(Err(CursorError::from_reqwest(error)));
                    }
                    // Clean EOF or idle (server waiting for a tool exec-result we
                    // never send): finish the turn either way.
                    Ok(None) | Err(_) => {
                        state.finished = true;
                        state.pending.push_back(Ok(CursorStreamEvent::End));
                    }
                }
            }
        })
    }
}

struct ReadState {
    bytes: futures_util::stream::BoxStream<'static, reqwest::Result<Bytes>>,
    decoder: ConnectFrameDecoder,
    pending: VecDeque<Result<CursorStreamEvent, CursorError>>,
    _guard: TurnGuard,
    got_text: bool,
    finished: bool,
}

impl ReadState {
    /// Decode Connect frames from a response chunk into pending events.
    fn ingest(&mut self, chunk: &[u8]) {
        let frames = match self.decoder.push(chunk) {
            Ok(frames) => frames,
            Err(error) => {
                self.finished = true;
                self.pending
                    .push_back(Err(CursorError::internal(format!("cursor frame: {error}"))));
                return;
            }
        };
        for frame in frames {
            if frame.flags & FLAG_END != 0 {
                if let Some(error) = parse_connect_error(&frame.payload) {
                    self.finished = true;
                    self.pending.push_back(Err(CursorError::new(
                        error.status,
                        error.to_string(),
                        Some(error.detail.clone()),
                    )));
                } else {
                    self.finished = true;
                    self.pending.push_back(Ok(CursorStreamEvent::End));
                }
                return;
            }
            let payload = if frame.flags & FLAG_GZIP != 0 {
                match decode_gzip_frame(&frame.payload) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        self.finished = true;
                        self.pending
                            .push_back(Err(CursorError::internal(format!("cursor gzip: {error}"))));
                        return;
                    }
                }
            } else {
                frame.payload.to_vec()
            };
            if let Some(text) = extract_reasoning_text(&payload) {
                self.pending
                    .push_back(Ok(CursorStreamEvent::ThinkingDelta { text }));
            }
            if let Some(text) = extract_answer_text(&payload) {
                self.got_text = true;
                self.pending
                    .push_back(Ok(CursorStreamEvent::TextDelta { text }));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Protobuf + Connect framing helpers (hand-rolled to match the captured wire)
// ---------------------------------------------------------------------------

fn encode_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push(((value as u8) & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Encode a length-delimited (wire type 2) protobuf field.
fn field_ld(field: u64, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 4);
    encode_varint((field << 3) | 2, &mut out);
    encode_varint(data.len() as u64, &mut out);
    out.extend_from_slice(data);
    out
}

/// Encode a varint (wire type 0) protobuf field.
fn field_varint(field: u64, value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    encode_varint(field << 3, &mut out);
    encode_varint(value, &mut out);
    out
}

fn field_str(field: u64, s: &str) -> Vec<u8> {
    field_ld(field, s.as_bytes())
}

/// Wrap a protobuf payload in an uncompressed Connect data frame.
fn connect_frame(payload: &[u8]) -> Bytes {
    crate::adapters::cursor::connect::encode_connect_frame(payload, 0)
}

/// `{f1: name, f3: {f1:'fast', f2:'true'|'false'}}` model descriptor.
fn encode_model_meta(name: &str, fast: bool) -> Vec<u8> {
    let mut out = field_str(1, name);
    let mut kv = field_str(1, "fast");
    kv.extend(field_str(2, if fast { "true" } else { "false" }));
    out.extend(field_ld(3, &kv));
    out
}

/// Build the ordered Connect frames for a single text prompt turn.
fn build_run_frames(prompt: &str, model: &str, cwd: &str) -> Vec<Bytes> {
    let conv = uuid::Uuid::new_v4().to_string();
    let msg = uuid::Uuid::new_v4().to_string();

    // frame 0: field 1 = RunRequest.
    // messages: f2 { f1 { f1 { f1:prompt, f2:msg_id, f3:'', f4:1 } } }
    let mut inner = field_str(1, prompt);
    inner.extend(field_str(2, &msg));
    inner.extend(field_str(3, ""));
    inner.extend(field_varint(4, 1));
    let messages = field_ld(2, &field_ld(1, &field_ld(1, &inner)));

    let mut req = field_str(1, "");
    req.extend(messages);
    req.extend(field_str(4, ""));
    req.extend(field_str(5, &conv));
    req.extend(field_ld(9, &encode_model_meta(model, false)));
    req.extend(field_varint(12, 0));
    // minimal catalog: a "default" entry plus the target model.
    req.extend(field_ld(14, &field_str(1, "default")));
    req.extend(field_ld(14, &encode_model_meta(model, false)));
    req.extend(field_str(16, &conv));
    let frame0 = connect_frame(&field_ld(1, &req));

    // frame 1: field 2 = environment context (env block only, no tools/skills).
    let mut env = field_str(1, "linux");
    env.extend(field_str(2, cwd));
    env.extend(field_str(3, "bash"));
    env.extend(field_str(10, "UTC"));
    env.extend(field_str(11, cwd));
    env.extend(field_varint(14, 1));
    env.extend(field_varint(16, 1));
    env.extend(field_varint(19, 0));
    env.extend(field_varint(20, 0));
    env.extend(field_str(21, cwd));
    env.extend(field_varint(22, 0));
    let ctx = field_ld(
        2,
        &field_ld(10, &field_ld(1, &field_ld(1, &field_ld(4, &env)))),
    );
    let frame1 = connect_frame(&ctx);

    let mut frames = vec![frame0, frame1];
    frames.push(connect_frame(&field_ld(5, &field_str(1, "")))); // f5{f1:''}
    frames.push(connect_frame(&field_ld(3, &field_str(3, "")))); // f3{f3:''}
    for n in 1..=8u64 {
        let mut m = field_varint(1, n);
        m.extend(field_str(3, ""));
        frames.push(connect_frame(&field_ld(3, &m))); // f3{f1:N, f3:''}
    }
    frames
}

/// A single `f7:''` heartbeat frame.
fn heartbeat_frame() -> Bytes {
    connect_frame(&field_ld(7, &[]))
}

// ---------------------------------------------------------------------------
// Response protobuf extraction
// ---------------------------------------------------------------------------

struct PbField<'a> {
    field: u64,
    wire: u8,
    data: &'a [u8],
}

fn iter_fields(mut buf: &[u8]) -> impl Iterator<Item = PbField<'_>> {
    std::iter::from_fn(move || {
        if buf.is_empty() {
            return None;
        }
        let (tag, rest) = read_varint(buf)?;
        let field = tag >> 3;
        let wire = (tag & 7) as u8;
        buf = rest;
        match wire {
            0 => {
                let (_v, rest) = read_varint(buf)?;
                buf = rest;
                Some(PbField {
                    field,
                    wire,
                    data: &[],
                })
            }
            2 => {
                let (len, rest) = read_varint(buf)?;
                let len = len as usize;
                if rest.len() < len {
                    return None;
                }
                let data = &rest[..len];
                buf = &rest[len..];
                Some(PbField { field, wire, data })
            }
            5 => {
                if buf.len() < 4 {
                    return None;
                }
                buf = &buf[4..];
                Some(PbField {
                    field,
                    wire,
                    data: &[],
                })
            }
            1 => {
                if buf.len() < 8 {
                    return None;
                }
                buf = &buf[8..];
                Some(PbField {
                    field,
                    wire,
                    data: &[],
                })
            }
            _ => None,
        }
    })
}

fn read_varint(buf: &[u8]) -> Option<(u64, &[u8])> {
    let mut result = 0u64;
    let mut shift = 0u32;
    for (i, &byte) in buf.iter().enumerate() {
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((result, &buf[i + 1..]));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    None
}

/// Extract a nested `f1.f<inner>.f1` string leaf from a response message payload.
fn extract_nested_text(payload: &[u8], inner: u64) -> Option<String> {
    for f1 in iter_fields(payload) {
        if f1.field != 1 || f1.wire != 2 {
            continue;
        }
        for mid in iter_fields(f1.data) {
            if mid.field != inner || mid.wire != 2 {
                continue;
            }
            for leaf in iter_fields(mid.data) {
                if leaf.field == 1 && leaf.wire == 2 {
                    if let Ok(text) = std::str::from_utf8(leaf.data) {
                        if !text.is_empty() {
                            return Some(text.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

/// Assistant answer text delta: `f1.f1.f1` string.
fn extract_answer_text(payload: &[u8]) -> Option<String> {
    extract_nested_text(payload, 1)
}

/// Reasoning text delta: `f1.f4.f1` string.
fn extract_reasoning_text(payload: &[u8]) -> Option<String> {
    extract_nested_text(payload, 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_well_formed_connect_frames() {
        let frames = build_run_frames("hi", "gpt-5.6-sol-high", "/tmp");
        assert!(frames.len() >= 4);
        for frame in &frames {
            assert!(frame.len() >= 5);
            let len = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
            assert_eq!(
                len + 5,
                frame.len(),
                "frame length prefix must match payload"
            );
            assert_eq!(frame[0], 0, "request frames are uncompressed data frames");
        }
    }

    #[test]
    fn frame0_contains_prompt_and_model() {
        let frames = build_run_frames("PROMPT_MARKER", "MODEL_MARKER", "/tmp");
        let hay = String::from_utf8_lossy(&frames[0]);
        assert!(hay.contains("PROMPT_MARKER"));
        assert!(hay.contains("MODEL_MARKER"));
    }

    #[test]
    fn heartbeat_is_a_single_connect_frame() {
        let frame = heartbeat_frame();
        assert_eq!(frame[0], 0);
        let len = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
        assert_eq!(len + 5, frame.len());
    }

    #[test]
    fn extract_answer_text_reads_nested_chunk() {
        // f1 { f1 { f1: "OK" } }
        let leaf = field_str(1, "OK");
        let mid = field_ld(1, &leaf);
        let top = field_ld(1, &mid);
        assert_eq!(extract_answer_text(&top).as_deref(), Some("OK"));
        // Answer extraction must ignore reasoning (f1.f4.f1).
        assert_eq!(extract_answer_text(&reasoning_payload("think")), None);
    }

    #[test]
    fn extract_reasoning_text_reads_f4_chunk() {
        assert_eq!(
            extract_reasoning_text(&reasoning_payload("think")).as_deref(),
            Some("think")
        );
        // Reasoning extraction must ignore the answer chunk (f1.f1.f1).
        let answer = field_ld(1, &field_ld(1, &field_str(1, "OK")));
        assert_eq!(extract_reasoning_text(&answer), None);
    }

    fn reasoning_payload(text: &str) -> Vec<u8> {
        field_ld(1, &field_ld(4, &field_str(1, text)))
    }
}
