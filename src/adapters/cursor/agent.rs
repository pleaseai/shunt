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
//! captured it from the real `cursor-agent` CLI. Assistant text and reasoning,
//! native MCP tool calls, and inline image context are all mapped to this wire
//! format; Cursor's own agentic file/shell tools are not exposed.

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
    decode_gzip_frame, parse_connect_error, ConnectError, ConnectFrameDecoder, FLAG_END, FLAG_GZIP,
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
/// expects a tool exec-result (which this stateless bridge never sends), so
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
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
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

/// An inline image attached to the current user message. `data` is the raw
/// (already base64-decoded) image bytes.
pub struct AgentImage {
    pub data: Vec<u8>,
    pub uuid: String,
    pub path: String,
    pub mime_type: String,
}

/// A client tool advertised to Cursor as a native MCP tool. The model invokes it
/// on the exec channel (`McpArgs`), which the reader surfaces as
/// [`CursorStreamEvent::ToolCall`].
pub struct AgentTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Inputs for a single agent turn.
pub struct AgentRunParams<'a> {
    pub prompt: &'a str,
    pub model_id: &'a str,
    pub cwd: &'a str,
    /// `AgentMode` enum (`UserMessage.mode`): AGENT=1, ASK=2, PLAN=3.
    pub mode: u64,
    pub images: &'a [AgentImage],
    pub tools: &'a [AgentTool],
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
        params: &AgentRunParams<'_>,
    ) -> Result<CursorAgentTurn, CursorError> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let frames = build_run_frames(params);

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

#[cfg(test)]
impl CursorAgentTurn {
    pub(super) fn from_response_for_test(response: reqwest::Response) -> Self {
        let (stop_tx, _stop_rx) = tokio::sync::oneshot::channel();
        let sender = tokio::spawn(async {});
        Self {
            response,
            guard: TurnGuard {
                _stop: stop_tx,
                _sender: sender,
            },
        }
    }
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
                    // Clean EOF: the turn ended. Validate the decoder finished on
                    // a frame boundary so a truncated body is an error, not a
                    // partial/empty success.
                    Ok(None) => {
                        state.finished = true;
                        state.pending.push_back(terminal_event(
                            false,
                            state.got_text,
                            state.decoder.finish(),
                        ));
                    }
                    // Budget elapsed. After output this is a normal idle end
                    // (the server keeps the response open waiting for a tool
                    // exec-result we never send); before any output it is a
                    // first-byte stall — surface that as an error, not an empty
                    // success.
                    Err(_) => {
                        state.finished = true;
                        state.pending.push_back(terminal_event(
                            true,
                            state.got_text,
                            state.decoder.finish(),
                        ));
                    }
                }
            }
        })
    }
}

/// Decide the terminal event when the upstream byte stream ends. A first-byte
/// timeout with no assistant output (`timed_out && !got_output`) is an upstream
/// stall and surfaces as an error rather than an empty success. Otherwise the
/// decoder must have ended on a frame boundary — leftover buffered bytes mean a
/// truncated body.
fn terminal_event(
    timed_out: bool,
    got_output: bool,
    finish: Result<(), ConnectError>,
) -> Result<CursorStreamEvent, CursorError> {
    if timed_out && !got_output {
        return Err(CursorError::internal(
            "cursor: upstream timed out before sending any output",
        ));
    }
    match finish {
        Ok(()) => Ok(CursorStreamEvent::End),
        Err(error) => Err(CursorError::internal(format!("cursor frame: {error}"))),
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
            let decompressed;
            let payload = if frame.flags & FLAG_GZIP != 0 {
                match decode_gzip_frame(&frame.payload) {
                    Ok(bytes) => {
                        decompressed = bytes;
                        &decompressed[..]
                    }
                    Err(error) => {
                        self.finished = true;
                        self.pending
                            .push_back(Err(CursorError::internal(format!("cursor gzip: {error}"))));
                        return;
                    }
                }
            } else {
                &frame.payload[..]
            };
            // A native MCP tool call ends the assistant turn: the model now waits
            // for an exec-result on the stream. The stateless bridge surfaces the
            // call as a tool_use pause and re-runs with the result in history, so
            // finish the turn here rather than sending an exec-result back.
            if let Some((name, input_json)) = extract_tool_call(payload) {
                self.got_text = true;
                self.finished = true;
                self.pending
                    .push_back(Ok(CursorStreamEvent::ToolCall { name, input_json }));
                return;
            }
            if let Some(text) = extract_reasoning_text(payload) {
                // Reasoning is upstream output too: once it arrives, switch from
                // the first-byte budget to the idle budget so a reasoning-only
                // turn that goes quiet isn't held for the full first-byte window.
                self.got_text = true;
                self.pending
                    .push_back(Ok(CursorStreamEvent::ThinkingDelta { text }));
            }
            if let Some(text) = extract_answer_text(payload) {
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

/// Encode a fixed64 (wire type 1) protobuf field carrying an f64 (little-endian).
fn field_double(field: u64, value: f64) -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    encode_varint((field << 3) | 1, &mut out);
    out.extend_from_slice(&value.to_le_bytes());
    out
}

/// Encode a JSON value as a `google.protobuf.Value` — the shape Cursor expects
/// for `McpToolDefinition.input_schema` (bytes, not JSON text). Value oneof
/// tags: null=1 (varint), number=2 (double), string=3, bool=4 (varint),
/// struct=5, list=6.
fn encode_protobuf_value(value: &serde_json::Value) -> Vec<u8> {
    match value {
        serde_json::Value::Null => field_varint(1, 0),
        serde_json::Value::Bool(b) => field_varint(4, u64::from(*b)),
        serde_json::Value::Number(n) => field_double(2, n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => field_str(3, s),
        serde_json::Value::Array(items) => {
            // Value.list_value (6) → ListValue { values = 1 (repeated Value) }.
            let mut list = Vec::new();
            for item in items {
                list.extend(field_ld(1, &encode_protobuf_value(item)));
            }
            field_ld(6, &list)
        }
        serde_json::Value::Object(map) => {
            // Value.struct_value (5) → Struct { fields = 1 (map<string,Value>) }.
            let mut fields = Vec::new();
            for (key, val) in map {
                let mut entry = field_str(1, key);
                entry.extend(field_ld(2, &encode_protobuf_value(val)));
                fields.extend(field_ld(1, &entry));
            }
            field_ld(5, &fields)
        }
    }
}

/// Encode one `McpToolDefinition { name=1, description=2, input_schema=3 (Value
/// bytes), provider_identifier=4, tool_name=5 }`.
fn encode_mcp_tool_def(tool: &AgentTool) -> Vec<u8> {
    let mut out = field_str(1, &tool.name);
    out.extend(field_str(2, &tool.description));
    out.extend(field_ld(3, &encode_protobuf_value(&tool.input_schema)));
    out.extend(field_str(4, "shunt"));
    out.extend(field_str(5, &tool.name));
    out
}

/// Encode the `McpTools { tools = 1 (repeated McpToolDefinition) }` wrapper body.
/// Empty tools serialize to zero bytes, so `field_ld(4, &body)` matches the
/// text-only placeholder (`field_str(4, "")`) exactly — the field is observably
/// required even when empty.
fn encode_mcp_tools(tools: &[AgentTool]) -> Vec<u8> {
    let mut out = Vec::new();
    for tool in tools {
        out.extend(field_ld(1, &encode_mcp_tool_def(tool)));
    }
    out
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

/// Encode one `SelectedImage`: `{uuid=2, path=3, mime_type=7, data=8}` (inline
/// raw bytes at field 8, per the `agent.v1` descriptor).
fn encode_selected_image(image: &AgentImage) -> Vec<u8> {
    let mut out = field_str(2, &image.uuid);
    out.extend(field_str(3, &image.path));
    out.extend(field_str(7, &image.mime_type));
    out.extend(field_ld(8, &image.data));
    out
}

/// Encode `SelectedContext { selected_images = 1 (repeated) }`.
fn encode_selected_context(images: &[AgentImage]) -> Vec<u8> {
    let mut out = Vec::new();
    for image in images {
        out.extend(field_ld(1, &encode_selected_image(image)));
    }
    out
}

/// Build the ordered Connect frames for a single agent turn.
fn build_run_frames(params: &AgentRunParams<'_>) -> Vec<Bytes> {
    let AgentRunParams {
        prompt,
        model_id: model,
        cwd,
        mode,
        images,
        tools,
    } = params;
    let conv = uuid::Uuid::new_v4().to_string();
    let msg = uuid::Uuid::new_v4().to_string();

    // frame 0: field 1 = RunRequest.
    // messages: f2 { f1 { f1 { f1:prompt, f2:msg_id, f3:selected_context, f4:mode } } }
    let mut inner = field_str(1, prompt);
    inner.extend(field_str(2, &msg));
    if images.is_empty() {
        // Empty placeholder (no selected context), matching the text-only turn.
        inner.extend(field_str(3, ""));
    } else {
        inner.extend(field_ld(3, &encode_selected_context(images)));
    }
    inner.extend(field_varint(4, *mode));
    let messages = field_ld(2, &field_ld(1, &field_ld(1, &inner)));

    let mut req = field_str(1, "");
    req.extend(messages);
    // f4 = mcp_tools (McpTools wrapper). Empty tools encode to the same bytes as
    // the text-only placeholder.
    req.extend(field_ld(4, &encode_mcp_tools(tools)));
    req.extend(field_str(5, &conv));
    req.extend(field_ld(9, &encode_model_meta(model, false)));
    // f12 is a workspace-context flag on the current wire (setting it to 1 makes
    // the server reject with "workspace context exclusion is not allowed"), so
    // keep it 0. Inline images ride in `selected_context` (f3) alone.
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
                let len = usize::try_from(len).ok()?;
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

/// Decode a native MCP tool call from a response message:
/// `exec_server_message(2) → ExecServerMessage.mcp_args(11) → McpArgs`.
/// Returns `(tool name, input JSON)`; `tool_name`(5) wins over `name`(1).
fn extract_tool_call(payload: &[u8]) -> Option<(String, String)> {
    for esm in iter_fields(payload) {
        // AgentServerMessage.exec_server_message = field 2.
        if esm.field != 2 || esm.wire != 2 {
            continue;
        }
        for args in iter_fields(esm.data) {
            // ExecServerMessage.mcp_args = field 11.
            if args.field == 11 && args.wire == 2 {
                if let Some(call) = decode_mcp_args(args.data) {
                    return Some(call);
                }
            }
        }
    }
    None
}

/// Decode `McpArgs { name=1, args=2 (map<string,Value>), tool_call_id=3,
/// tool_name=5 }` into `(name, input JSON)`. `tool_call_id` is intentionally
/// ignored — the Anthropic tool_use id is minted by the caller.
fn decode_mcp_args(buf: &[u8]) -> Option<(String, String)> {
    let mut name: Option<String> = None;
    let mut tool_name: Option<String> = None;
    let mut args = serde_json::Map::new();
    for field in iter_fields(buf) {
        if field.wire != 2 {
            continue;
        }
        match field.field {
            1 => name = std::str::from_utf8(field.data).ok().map(str::to_string),
            5 => tool_name = std::str::from_utf8(field.data).ok().map(str::to_string),
            2 => {
                // One map<string,Value> entry: { key=1, value=2 (Value) }.
                let mut key: Option<String> = None;
                let mut value: Option<&[u8]> = None;
                for entry in iter_fields(field.data) {
                    match (entry.field, entry.wire) {
                        (1, 2) => key = std::str::from_utf8(entry.data).ok().map(str::to_string),
                        (2, 2) => value = Some(entry.data),
                        _ => {}
                    }
                }
                if let (Some(key), Some(value)) = (key, value) {
                    args.insert(key, decode_protobuf_value(value));
                }
            }
            _ => {}
        }
    }
    let name = tool_name.or(name)?;
    let input_json = serde_json::to_string(&serde_json::Value::Object(args)).ok()?;
    Some((name, input_json))
}

/// Maximum `google.protobuf.Value` nesting shunt will decode. Bounds recursion
/// so a hostile/malformed deeply-nested payload cannot overflow the stack
/// (mirrors the gzip-size cap in `connect.rs`). Nesting past the cap decodes to
/// `Null`.
const MAX_PROTOBUF_VALUE_DEPTH: usize = 64;

/// Decode a `google.protobuf.Value` (exactly one oneof field set) into JSON.
fn decode_protobuf_value(buf: &[u8]) -> serde_json::Value {
    decode_protobuf_value_at(buf, 0)
}

fn decode_protobuf_value_at(buf: &[u8], depth: usize) -> serde_json::Value {
    if depth >= MAX_PROTOBUF_VALUE_DEPTH {
        return serde_json::Value::Null;
    }
    let Some((tag, rest)) = read_varint(buf) else {
        return serde_json::Value::Null;
    };
    let field = tag >> 3;
    let wire = (tag & 7) as u8;
    match (field, wire) {
        // null_value (varint).
        (1, 0) => serde_json::Value::Null,
        // number_value (fixed64 double, little-endian).
        (2, 1) => rest
            .get(..8)
            .and_then(|b| b.try_into().ok())
            .map(f64::from_le_bytes)
            .and_then(serde_json::Number::from_f64)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        // string_value.
        (3, 2) => read_varint(rest)
            .and_then(|(len, body)| body.get(..usize::try_from(len).ok()?))
            .map(|s| serde_json::Value::String(String::from_utf8_lossy(s).into_owned()))
            .unwrap_or(serde_json::Value::Null),
        // bool_value (varint).
        (4, 0) => match read_varint(rest) {
            Some((v, _)) => serde_json::Value::Bool(v != 0),
            None => serde_json::Value::Null,
        },
        // struct_value.
        (5, 2) => read_varint(rest)
            .and_then(|(len, body)| body.get(..usize::try_from(len).ok()?))
            .map(|body| decode_protobuf_struct_at(body, depth + 1))
            .unwrap_or(serde_json::Value::Null),
        // list_value.
        (6, 2) => read_varint(rest)
            .and_then(|(len, body)| body.get(..usize::try_from(len).ok()?))
            .map(|body| decode_protobuf_list_at(body, depth + 1))
            .unwrap_or(serde_json::Value::Null),
        _ => serde_json::Value::Null,
    }
}

/// Decode `google.protobuf.Struct { fields = 1 (map<string,Value>) }`.
fn decode_protobuf_struct_at(buf: &[u8], depth: usize) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for field in iter_fields(buf) {
        if field.field != 1 || field.wire != 2 {
            continue;
        }
        let mut key: Option<String> = None;
        let mut value: Option<&[u8]> = None;
        for entry in iter_fields(field.data) {
            match (entry.field, entry.wire) {
                (1, 2) => key = std::str::from_utf8(entry.data).ok().map(str::to_string),
                (2, 2) => value = Some(entry.data),
                _ => {}
            }
        }
        if let (Some(key), Some(value)) = (key, value) {
            map.insert(key, decode_protobuf_value_at(value, depth));
        }
    }
    serde_json::Value::Object(map)
}

/// Decode `google.protobuf.ListValue { values = 1 (repeated Value) }`.
fn decode_protobuf_list_at(buf: &[u8], depth: usize) -> serde_json::Value {
    let mut items = Vec::new();
    for field in iter_fields(buf) {
        if field.field == 1 && field.wire == 2 {
            items.push(decode_protobuf_value_at(field.data, depth));
        }
    }
    serde_json::Value::Array(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_params<'a>(prompt: &'a str, model: &'a str, cwd: &'a str) -> AgentRunParams<'a> {
        AgentRunParams {
            prompt,
            model_id: model,
            cwd,
            mode: 1,
            images: &[],
            tools: &[],
        }
    }

    #[test]
    fn terminal_event_first_byte_timeout_without_output_is_error() {
        assert!(super::terminal_event(true, false, Ok(())).is_err());
    }

    #[test]
    fn terminal_event_idle_timeout_after_output_ends_cleanly() {
        assert!(matches!(
            super::terminal_event(true, true, Ok(())),
            Ok(CursorStreamEvent::End)
        ));
    }

    #[test]
    fn terminal_event_clean_eof_ends_even_without_output() {
        assert!(matches!(
            super::terminal_event(false, false, Ok(())),
            Ok(CursorStreamEvent::End)
        ));
    }

    #[test]
    fn terminal_event_truncated_frame_is_error() {
        let finish =
            Err(crate::adapters::cursor::connect::ConnectError::TruncatedFrame { buffered: 3 });
        assert!(super::terminal_event(false, true, finish).is_err());
    }

    #[test]
    fn frames_are_well_formed_connect_frames() {
        let frames = build_run_frames(&text_params("hi", "gpt-5.6-sol-high", "/tmp"));
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
        let frames = build_run_frames(&text_params("PROMPT_MARKER", "MODEL_MARKER", "/tmp"));
        let hay = String::from_utf8_lossy(&frames[0]);
        assert!(hay.contains("PROMPT_MARKER"));
        assert!(hay.contains("MODEL_MARKER"));
    }

    #[test]
    fn mode_enum_is_encoded_in_user_message() {
        // f4 (mode) inside the user message carries the AgentMode enum. A Plan
        // turn (3) must differ from an Agent turn (1) in the frame-0 bytes.
        let agent = build_run_frames(&AgentRunParams {
            prompt: "x",
            model_id: "m",
            cwd: "/tmp",
            mode: 1,
            images: &[],
            tools: &[],
        });
        let plan = build_run_frames(&AgentRunParams {
            prompt: "x",
            model_id: "m",
            cwd: "/tmp",
            mode: 3,
            images: &[],
            tools: &[],
        });
        assert_ne!(agent[0], plan[0], "mode must change the request bytes");
    }

    #[test]
    fn images_populate_selected_context() {
        let image = AgentImage {
            data: vec![1, 2, 3, 4],
            uuid: "IMG_UUID".into(),
            path: "claude-image-1.png".into(),
            mime_type: "image/png".into(),
        };
        let frames = build_run_frames(&AgentRunParams {
            prompt: "look",
            model_id: "m",
            cwd: "/tmp",
            mode: 1,
            images: std::slice::from_ref(&image),
            tools: &[],
        });
        let hay = String::from_utf8_lossy(&frames[0]);
        // uuid, path and mime are string fields of the SelectedImage.
        assert!(hay.contains("IMG_UUID"));
        assert!(hay.contains("claude-image-1.png"));
        assert!(hay.contains("image/png"));
        // The empty-context turn and the image turn must differ.
        let empty = build_run_frames(&text_params("look", "m", "/tmp"));
        assert_ne!(empty[0], frames[0]);
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

    #[test]
    fn tools_are_encoded_in_request() {
        let tool = AgentTool {
            name: "Read".into(),
            description: "Read a file".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"file_path": {"type": "string"}}
            }),
        };
        let frames = build_run_frames(&AgentRunParams {
            prompt: "x",
            model_id: "m",
            cwd: "/tmp",
            mode: 1,
            images: &[],
            tools: std::slice::from_ref(&tool),
        });
        let hay = String::from_utf8_lossy(&frames[0]);
        assert!(hay.contains("Read"));
        assert!(hay.contains("Read a file"));
        // A declared tool must change the request bytes vs the no-tool turn.
        let empty = build_run_frames(&text_params("x", "m", "/tmp"));
        assert_ne!(empty[0], frames[0]);
    }

    #[test]
    fn empty_tools_match_text_placeholder_bytes() {
        // encode_mcp_tools(&[]) must serialize to zero bytes so field 4 matches
        // the text-only placeholder `field_str(4, "")` exactly.
        assert!(encode_mcp_tools(&[]).is_empty());
        assert_eq!(field_ld(4, &encode_mcp_tools(&[])), field_str(4, ""));
    }

    #[test]
    fn deeply_nested_value_is_bounded() {
        // Nest a value well past the depth cap; decoding must terminate (bounded
        // recursion) rather than overflow the stack.
        let mut value = serde_json::json!("leaf");
        for _ in 0..(super::MAX_PROTOBUF_VALUE_DEPTH + 40) {
            value = serde_json::Value::Array(vec![value]);
        }
        let decoded = decode_protobuf_value(&encode_protobuf_value(&value));
        let mut cursor = &decoded;
        while let Some(inner) = cursor.as_array().and_then(|items| items.first()) {
            cursor = inner;
        }
        assert!(cursor.is_null() || cursor.is_array());
    }

    #[test]
    fn protobuf_value_round_trips_json() {
        let value = serde_json::json!({
            "a": 1, "b": "two", "c": [true, null, 3.5], "d": {"e": false}
        });
        let decoded = decode_protobuf_value(&encode_protobuf_value(&value));
        assert_eq!(decoded["a"].as_f64(), Some(1.0));
        assert_eq!(decoded["b"], serde_json::json!("two"));
        assert_eq!(decoded["c"][0], serde_json::json!(true));
        assert!(decoded["c"][1].is_null());
        assert_eq!(decoded["c"][2].as_f64(), Some(3.5));
        assert_eq!(decoded["d"]["e"], serde_json::json!(false));
    }

    #[test]
    fn extract_tool_call_decodes_mcp_args() {
        // AgentServerMessage(2) → ExecServerMessage.mcp_args(11) → McpArgs
        //   { tool_name=5: "Read", args=2: {file_path:"/tmp/x", limit:5} }
        let mut mcp_args = field_str(5, "Read");
        for (key, value) in [
            ("file_path", serde_json::json!("/tmp/x")),
            ("limit", serde_json::json!(5)),
        ] {
            let mut entry = field_str(1, key);
            entry.extend(field_ld(2, &encode_protobuf_value(&value)));
            mcp_args.extend(field_ld(2, &entry));
        }
        let payload = field_ld(2, &field_ld(11, &mcp_args));
        let (name, input_json) = extract_tool_call(&payload).expect("tool call decoded");
        assert_eq!(name, "Read");
        let input: serde_json::Value = serde_json::from_str(&input_json).unwrap();
        assert_eq!(input["file_path"], serde_json::json!("/tmp/x"));
        assert_eq!(input["limit"].as_f64(), Some(5.0));
    }

    #[test]
    fn tool_name_field_5_wins_over_name_field_1() {
        // McpArgs with both name(1) and tool_name(5): tool_name must win.
        let mut mcp_args = field_str(1, "fallback");
        mcp_args.extend(field_str(5, "Preferred"));
        let payload = field_ld(2, &field_ld(11, &mcp_args));
        let (name, _) = extract_tool_call(&payload).expect("tool call decoded");
        assert_eq!(name, "Preferred");
    }

    async fn turn_from_frames(frames: Vec<u8>) -> CursorAgentTurn {
        use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(frames, "application/connect+proto"),
            )
            .mount(&server)
            .await;
        let response = reqwest::Client::new()
            .get(server.uri())
            .send()
            .await
            .expect("mock response should be available");
        CursorAgentTurn::from_response_for_test(response)
    }

    fn success_end_frame() -> Bytes {
        crate::adapters::cursor::connect::encode_connect_frame(b"{}", FLAG_END)
    }

    fn tool_call_frame(name: &str, args: &[(&str, serde_json::Value)]) -> Bytes {
        let mut mcp_args = field_str(5, name);
        for (key, value) in args {
            let mut entry = field_str(1, key);
            entry.extend(field_ld(2, &encode_protobuf_value(value)));
            mcp_args.extend(field_ld(2, &entry));
        }
        connect_frame(&field_ld(2, &field_ld(11, &mcp_args)))
    }

    #[tokio::test]
    async fn event_stream_emits_text_and_end() {
        let mut frames = connect_frame(&field_ld(1, &field_ld(1, &field_str(1, "hello")))).to_vec();
        frames.extend_from_slice(&success_end_frame());

        let events: Vec<_> = turn_from_frames(frames)
            .await
            .into_event_stream()
            .collect()
            .await;

        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            Ok(CursorStreamEvent::TextDelta { text }) if text == "hello"
        ));
        assert!(matches!(&events[1], Ok(CursorStreamEvent::End)));
    }

    #[tokio::test]
    async fn event_stream_emits_reasoning_before_text() {
        let mut frames =
            connect_frame(&field_ld(1, &field_ld(4, &field_str(1, "thinking")))).to_vec();
        frames.extend_from_slice(&connect_frame(&field_ld(
            1,
            &field_ld(1, &field_str(1, "hello")),
        )));
        frames.extend_from_slice(&success_end_frame());

        let events: Vec<_> = turn_from_frames(frames)
            .await
            .into_event_stream()
            .collect()
            .await;

        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[0],
            Ok(CursorStreamEvent::ThinkingDelta { text }) if text == "thinking"
        ));
        assert!(matches!(
            &events[1],
            Ok(CursorStreamEvent::TextDelta { text }) if text == "hello"
        ));
        assert!(matches!(&events[2], Ok(CursorStreamEvent::End)));
    }

    #[tokio::test]
    async fn event_stream_emits_native_tool_call() {
        let mut frames = tool_call_frame(
            "Read",
            &[
                ("file_path", serde_json::json!("/tmp/x")),
                ("limit", serde_json::json!(5)),
            ],
        )
        .to_vec();
        frames.extend_from_slice(&success_end_frame());

        let events: Vec<_> = turn_from_frames(frames)
            .await
            .into_event_stream()
            .collect()
            .await;

        let (name, input_json) = events
            .iter()
            .find_map(|event| match event {
                Ok(CursorStreamEvent::ToolCall { name, input_json }) => Some((name, input_json)),
                _ => None,
            })
            .expect("tool call event should be present");
        assert_eq!(name, "Read");
        let input: serde_json::Value = serde_json::from_str(input_json).unwrap();
        assert_eq!(input["file_path"], serde_json::json!("/tmp/x"));
        assert_eq!(input["limit"].as_f64(), Some(5.0));
    }

    #[tokio::test]
    async fn event_stream_surfaces_connect_end_error() {
        let frame = crate::adapters::cursor::connect::encode_connect_frame(
            br#"{"error":{"code":"unauthenticated","message":"bad"}}"#,
            FLAG_END,
        );

        let events: Vec<_> = turn_from_frames(frame.to_vec())
            .await
            .into_event_stream()
            .collect()
            .await;

        let error = events
            .last()
            .expect("error event should be present")
            .as_ref()
            .expect_err("Connect error should fail the turn");
        assert_eq!(error.status, 401);
        assert!(error.message.contains("bad"));
    }

    #[tokio::test]
    async fn test_turn_exposes_status_and_raw_response() {
        let turn = turn_from_frames(Vec::new()).await;
        assert_eq!(turn.status(), reqwest::StatusCode::OK);
        assert_eq!(turn.into_response().status(), reqwest::StatusCode::OK);
    }

    #[tokio::test]
    async fn event_stream_treats_clean_eof_as_end() {
        let events: Vec<_> = turn_from_frames(Vec::new())
            .await
            .into_event_stream()
            .collect()
            .await;

        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], Ok(CursorStreamEvent::End)));
    }

    #[tokio::test]
    async fn event_stream_decodes_gzipped_text() {
        use std::io::Write;

        let payload = field_ld(1, &field_ld(1, &field_str(1, "compressed")));
        let mut compressed = Vec::new();
        let mut encoder =
            flate2::write::GzEncoder::new(&mut compressed, flate2::Compression::fast());
        encoder.write_all(&payload).unwrap();
        encoder.finish().unwrap();
        let mut frames =
            crate::adapters::cursor::connect::encode_connect_frame(compressed, FLAG_GZIP).to_vec();
        frames.extend_from_slice(&success_end_frame());

        let events: Vec<_> = turn_from_frames(frames)
            .await
            .into_event_stream()
            .collect()
            .await;

        assert!(matches!(
            &events[0],
            Ok(CursorStreamEvent::TextDelta { text }) if text == "compressed"
        ));
        assert!(matches!(&events[1], Ok(CursorStreamEvent::End)));
    }

    #[tokio::test]
    async fn event_stream_rejects_invalid_gzip() {
        let frame = crate::adapters::cursor::connect::encode_connect_frame(b"not-gzip", FLAG_GZIP);

        let events: Vec<_> = turn_from_frames(frame.to_vec())
            .await
            .into_event_stream()
            .collect()
            .await;

        let error = events[0].as_ref().expect_err("invalid gzip should fail");
        assert!(error.message.contains("cursor gzip"));
    }

    #[tokio::test]
    async fn event_stream_rejects_oversized_frame_header() {
        // 0x04000001 is one byte over the decoder's 64 MiB payload limit.
        let events: Vec<_> = turn_from_frames(vec![0, 4, 0, 0, 1])
            .await
            .into_event_stream()
            .collect()
            .await;

        let error = events[0].as_ref().expect_err("oversized frame should fail");
        assert!(error.message.contains("cursor frame"));
    }

    #[test]
    fn protobuf_field_iterator_handles_all_supported_wire_types() {
        let mut encoded = field_varint(1, 300);
        encoded.extend(field_double(2, 1.5));
        encoded.extend(field_str(3, "value"));
        encoded.extend([0x25, 1, 2, 3, 4]); // field 4, fixed32 (wire 5)

        let fields: Vec<_> = iter_fields(&encoded)
            .map(|field| (field.field, field.wire, field.data.to_vec()))
            .collect();

        assert_eq!(fields.len(), 4);
        assert_eq!((fields[0].0, fields[0].1), (1, 0));
        assert_eq!((fields[1].0, fields[1].1), (2, 1));
        assert_eq!((fields[2].0, fields[2].1), (3, 2));
        assert_eq!(fields[2].2, b"value");
        assert_eq!((fields[3].0, fields[3].1), (4, 5));
    }

    #[test]
    fn protobuf_parsers_reject_truncated_and_unknown_fields() {
        for malformed in [
            vec![0x0a, 5, b'x'],
            vec![0x0d, 1],
            vec![0x09, 1],
            vec![0x0f],
        ] {
            assert_eq!(iter_fields(&malformed).count(), 0);
        }
        assert!(read_varint(&[]).is_none());
        assert!(read_varint(&[0x80; 10]).is_none());

        for malformed_value in [
            Vec::new(),
            vec![0x1a, 5, b'x'],
            vec![0x20],
            vec![0x2a, 5, b'x'],
            vec![0x32, 5, b'x'],
            vec![0x3a],
        ] {
            assert!(decode_protobuf_value(&malformed_value).is_null());
        }
    }
}
