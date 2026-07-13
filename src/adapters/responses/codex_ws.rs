//! Codex Responses WebSocket v2 transport (issue #32).
//!
//! The ChatGPT/Codex backend exposes the Responses API over a WebSocket
//! (`wss://…/codex/responses`) negotiated with the beta protocol header
//! `openai-beta: responses_websockets=2026-02-06`. Real Codex uses it to keep a
//! connection warm across a turn and reuse `previous_response_id`, which trims
//! per-turn upload and dodges the ~372k HTTP request ceiling that silently
//! drops. This module is the transport layer only: it opens the socket, sends a
//! single `response.create` frame, and streams the backend's events back as
//! [`ResponseEvent`]s so the existing [`crate::model::responses::AnthropicSseMachine`]
//! can translate them exactly as it does the HTTP SSE stream.
//!
//! Connections are pooled per `x-claude-code-session-id`, so turns of one
//! conversation reuse a live socket instead of re-handshaking. On a reused
//! connection this module also records the completed turn's response id and
//! output items as [`StoredContinuation`], so the next turn can replay
//! `previous_response_id` and upload only the input delta (the decision itself
//! lives in [`crate::adapters::responses::codex_continuation`]). `previous_response_id` is
//! only ever valid on the exact connection that produced it, which is why the
//! continuation state is stored on the [`PoolEntry`] rather than globally.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, StatusCode};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex as AsyncMutex, OwnedMutexGuard};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::codex_continuation::{build_transcript, StoredContinuation};
use crate::model::responses::ResponseEvent;

/// Header the backend uses to hand back (and codex echoes back) the per-turn
/// state token.
const TURN_STATE_HEADER: &str = "x-codex-turn-state";

/// Beta protocol value that selects the Responses WebSocket v2 endpoint. Mirrors
/// `RESPONSES_WEBSOCKETS_V2_BETA_HEADER_VALUE` in openai/codex.
pub const WEBSOCKET_BETA_PROTOCOL: &str = "responses_websockets=2026-02-06";

/// How long to wait for the WebSocket handshake to complete.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Idle ceiling between frames once connected. Reset on every frame (including
/// keepalive pings), so a healthy but slow generation never trips it.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Buffer for the event channel; backpressures the reader if the client is slow.
const EVENT_CHANNEL_CAPACITY: usize = 64;
/// How long a pooled connection may sit idle before it is evicted on the next
/// insert. Matches the reference proxy's 30-minute window.
const POOL_IDLE_TTL: Duration = Duration::from_secs(30 * 60);
/// How often ordinary inserts may trigger an idle-entry sweep. This keeps stale
/// sockets bounded without turning every insert into an O(pool size) operation.
const POOL_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// Hard cap on pooled connections, a backstop against unbounded session churn.
const MAX_POOL_ENTRIES: usize = 10_000;

/// WebSocket event types that end a response.
const TERMINAL_EVENTS: &[&str] = &[
    "response.completed",
    "response.incomplete",
    "response.failed",
    "error",
];

/// The only terminal event that leaves the connection healthy enough to reuse.
/// A failed/incomplete/error response may have left the socket in an undefined
/// state, so those are not pooled.
const REUSABLE_TERMINAL: &str = "response.completed";

/// The concrete websocket stream type (TLS or plaintext over TCP).
type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// A pooled, reusable websocket connection keyed by session id. The stream is
/// behind an async mutex so a turn holds it exclusively while streaming, then
/// releases it for the next turn on the same session. `continuation` carries the
/// state captured from this connection's previous turn, so `previous_response_id`
/// is only ever replayed on the connection that produced it.
struct PoolEntry {
    ws: std::sync::Arc<AsyncMutex<WsStream>>,
    last_used_at: Mutex<Instant>,
    continuation: Mutex<Option<StoredContinuation>>,
}

impl PoolEntry {
    fn new(stream: WsStream) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            ws: std::sync::Arc::new(AsyncMutex::new(stream)),
            last_used_at: Mutex::new(Instant::now()),
            continuation: Mutex::new(None),
        })
    }
}

/// Process-global connection pool keyed by `x-claude-code-session-id`. A std
/// mutex guards only map lookups/inserts (never held across an await); the
/// per-connection async mutex serializes turns on one session.
static POOL: LazyLock<Mutex<HashMap<String, std::sync::Arc<PoolEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static LAST_POOL_SWEEP: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));

fn pool_get(key: &str) -> Option<std::sync::Arc<PoolEntry>> {
    POOL.lock().unwrap().get(key).cloned()
}

/// Remove a session's pooled connection (called on staleness or any error).
pub fn invalidate_pool_key(key: &str) {
    POOL.lock().unwrap().remove(key);
}

fn pool_insert(key: String, entry: std::sync::Arc<PoolEntry>) {
    let mut guard = POOL.lock().unwrap();
    let mut last_sweep = LAST_POOL_SWEEP.lock().unwrap();
    let sweep_due = last_sweep.elapsed() >= POOL_SWEEP_INTERVAL;
    if sweep_due || guard.len() >= MAX_POOL_ENTRIES {
        // Sweep at most once per interval during ordinary churn, but always sweep
        // under capacity pressure before choosing an LRU victim.
        guard.retain(|_, entry| entry.last_used_at.lock().unwrap().elapsed() < POOL_IDLE_TTL);
        *last_sweep = Instant::now();
    }
    drop(last_sweep);
    if guard.len() >= MAX_POOL_ENTRIES {
        // Evict the least recently used connection. `HashMap` iteration order is
        // unspecified, so `keys().next()` would drop an arbitrary (possibly active)
        // entry instead of the stalest one.
        if let Some(oldest) = guard
            .iter()
            .min_by_key(|(_, entry)| *entry.last_used_at.lock().unwrap())
            .map(|(key, _)| key.clone())
        {
            guard.remove(&oldest);
        }
    }
    guard.insert(key, entry);
}

#[cfg(test)]
pub fn clear_pool_for_tests() {
    POOL.lock().unwrap().clear();
    *LAST_POOL_SWEEP.lock().unwrap() = Instant::now();
}

#[cfg(test)]
pub fn pool_contains_for_tests(key: &str) -> bool {
    POOL.lock().unwrap().contains_key(key)
}

/// A transport or upstream error surfaced by the websocket path. `status` and
/// `retry_after` are populated from the HTTP upgrade response when the handshake
/// itself fails (401/403/429), so the adapter can re-shape it identically to the
/// HTTP path (`mapped_upstream_error`).
#[derive(Debug, Clone)]
pub struct CodexWsError {
    /// HTTP status from a failed upgrade handshake, when one was returned.
    pub status: Option<StatusCode>,
    /// `retry-after` header from a failed upgrade handshake, when present.
    pub retry_after: Option<String>,
    /// Upstream body text from a failed handshake (may be empty).
    pub body: String,
    /// Internal, non-user-facing description for logs.
    pub message: String,
    /// Set when the backend rejected a replayed `previous_response_id`
    /// (`previous_response_not_found`), so the caller can retry with full input.
    pub previous_response_missing: bool,
}

impl CodexWsError {
    fn transport(message: impl Into<String>) -> Self {
        Self {
            status: None,
            retry_after: None,
            body: String::new(),
            message: message.into(),
            previous_response_missing: false,
        }
    }

    fn previous_response_missing() -> Self {
        Self {
            previous_response_missing: true,
            ..Self::transport("previous_response_not_found")
        }
    }
}

/// Receiver of translated events, terminated by `None`. A single `Err` item ends
/// the stream (the reader stops after sending it).
pub type CodexWsEvents = mpsc::Receiver<Result<ResponseEvent, CodexWsError>>;

/// Rewrite an `http(s)` Responses URL to its `ws(s)` equivalent. The backend
/// serves the websocket at the same path the HTTP adapter POSTs to.
pub fn to_websocket_url(url: &str) -> Result<String, CodexWsError> {
    if let Some(rest) = url.strip_prefix("https://") {
        Ok(format!("wss://{rest}"))
    } else if let Some(rest) = url.strip_prefix("http://") {
        Ok(format!("ws://{rest}"))
    } else if url.starts_with("ws://") || url.starts_with("wss://") {
        Ok(url.to_string())
    } else {
        Err(CodexWsError::transport(format!(
            "unsupported websocket url scheme: {url}"
        )))
    }
}

/// Build the `response.create` frame from a translated Responses request body.
/// The websocket envelope is the same request JSON tagged with
/// `"type": "response.create"` (see `ResponsesWsRequest` in openai/codex).
pub fn response_create_frame(mut body: Value) -> Value {
    if let Some(object) = body.as_object_mut() {
        object.insert("type".to_string(), Value::String("response.create".into()));
    }
    body
}

/// What to record as continuation state after a turn completes: the request's
/// non-input signature and its full logical input, so the reader can assemble
/// `input ++ output_items` for the next turn's prefix match.
pub struct RecordPlan {
    pub signature: String,
    pub request_input: Vec<Value>,
}

impl RecordPlan {
    /// A plan that records nothing (used when there is no session to continue).
    pub fn none() -> Self {
        Self {
            signature: String::new(),
            request_input: Vec::new(),
        }
    }
}

/// A connection acquired and locked for one turn, before its frame is sent.
/// Splitting acquire from send lets the caller inspect [`Turn::stored_continuation`]
/// (only present on a reused connection) and decide the `previous_response_id`
/// delta before committing the frame.
pub struct Turn {
    entry: std::sync::Arc<PoolEntry>,
    guard: OwnedMutexGuard<WsStream>,
    reused: bool,
    pool_key: Option<String>,
    handshake_turn_state: Option<String>,
}

impl Turn {
    /// The continuation state captured on this connection's previous turn.
    /// `None` for a fresh connection — `previous_response_id` is only valid on the
    /// connection that produced it.
    pub fn stored_continuation(&self) -> Option<StoredContinuation> {
        if !self.reused {
            return None;
        }
        self.entry.continuation.lock().unwrap().clone()
    }

    /// The `x-codex-turn-state` captured from the handshake, if any.
    pub fn handshake_turn_state(&self) -> Option<&str> {
        self.handshake_turn_state.as_deref()
    }

    /// Send the `response.create` frame and stream events back. The reader holds
    /// the connection for the whole turn and, on a clean completion, records new
    /// continuation state and returns the connection to the pool.
    pub async fn stream(
        self,
        frame: &Value,
        record: RecordPlan,
    ) -> Result<CodexWsEvents, CodexWsError> {
        let Turn {
            entry,
            mut guard,
            reused,
            pool_key,
            handshake_turn_state,
        } = self;
        let payload = serde_json::to_string(frame).map_err(|error| {
            CodexWsError::transport(format!("failed to encode ws frame: {error}"))
        })?;
        if let Err(error) = guard.send(Message::Text(payload)).await {
            // The liveness `Ping` in `begin` can false-positive on a half-open
            // socket (the send buffers locally). If the real frame send then fails
            // on a reused connection, evict it here so the next turn on this session
            // opens a fresh socket instead of re-probing the same dead entry until
            // its TTL expires. Mirrors the eviction in `begin` and `run_reader`.
            if reused {
                if let Some(key) = &pool_key {
                    invalidate_pool_key(key);
                }
            }
            return Err(CodexWsError::transport(format!(
                "websocket send failed: {error}"
            )));
        }
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        tokio::spawn(run_reader(
            ReaderCtx {
                entry,
                guard,
                reused,
                pool_key,
                record,
                handshake_turn_state,
            },
            tx,
        ));
        Ok(rx)
    }
}

/// Acquire a connection for `pool_key`, reusing a live pooled one (verified with a
/// liveness ping) or performing a fresh handshake. A stale pooled connection is
/// evicted and replaced. A refused handshake (401/403/429) resolves to `Err` with
/// the upstream status/body so the caller can re-shape it like the HTTP path.
pub async fn begin(
    ws_url: &str,
    headers: HeaderMap,
    pool_key: Option<&str>,
) -> Result<Turn, CodexWsError> {
    if let Some(key) = pool_key {
        if let Some(entry) = pool_get(key) {
            let mut guard = entry.ws.clone().lock_owned().await;
            // Cheap liveness probe: if the ping send fails the socket was dead
            // (commonly idle-closed), so evict and open a fresh handshake — where
            // the stored `previous_response_id` no longer applies.
            if guard.send(Message::Ping(Vec::new())).await.is_ok() {
                *entry.last_used_at.lock().unwrap() = Instant::now();
                return Ok(Turn {
                    entry,
                    guard,
                    reused: true,
                    pool_key: Some(key.to_string()),
                    handshake_turn_state: None,
                });
            }
            drop(guard);
            invalidate_pool_key(key);
        }
    }

    let (stream, handshake_turn_state) = connect(ws_url, headers).await?;
    let entry = PoolEntry::new(stream);
    let guard = entry.ws.clone().lock_owned().await;
    Ok(Turn {
        entry,
        guard,
        reused: false,
        pool_key: pool_key.map(str::to_string),
        handshake_turn_state,
    })
}

/// Install the rustls process-wide crypto provider on the first websocket
/// handshake. Feature unification compiles two providers into the binary —
/// aws-lc-rs (via sentry's reqwest `rustls` feature) and ring (via reqwest's own
/// `rustls-tls` feature) — so rustls 0.23 refuses to auto-select one, and
/// `tokio_tungstenite::connect_async` — which pulls tokio-rustls with no provider
/// feature of its own — panics without an installed default. Doing it here rather
/// than only in `main` covers the library target: integration tests and external
/// consumers reach this path without running `main`. `Once` keeps it idempotent
/// and race-free across concurrent first turns; pin aws-lc-rs to match reqwest/sentry.
fn ensure_crypto_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        // `install_default` errors only if a provider is already installed, which
        // is harmless — discard it rather than panicking.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// Perform the websocket handshake, mapping a refused upgrade to a status-bearing
/// [`CodexWsError`] and capturing the handshake `x-codex-turn-state` if present.
async fn connect(
    ws_url: &str,
    headers: HeaderMap,
) -> Result<(WsStream, Option<String>), CodexWsError> {
    ensure_crypto_provider();
    let mut request = ws_url
        .into_client_request()
        .map_err(|error| CodexWsError::transport(format!("invalid websocket request: {error}")))?;
    // `into_client_request` fills the mandatory upgrade headers (Host,
    // Connection, Upgrade, Sec-WebSocket-Key/Version); layer the Codex identity
    // and beta-protocol headers on top.
    request.headers_mut().extend(headers);

    let connect = tokio_tungstenite::connect_async(request);
    match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok((stream, response))) => {
            let turn_state = response
                .headers()
                .get(TURN_STATE_HEADER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            Ok((stream, turn_state))
        }
        Ok(Err(error)) => Err(map_handshake_error(error)),
        Err(_) => Err(CodexWsError::transport(format!(
            "websocket connect timed out after {}s",
            CONNECT_TIMEOUT.as_secs()
        ))),
    }
}

/// Everything the reader task needs to stream a turn and record its continuation.
struct ReaderCtx {
    entry: std::sync::Arc<PoolEntry>,
    guard: OwnedMutexGuard<WsStream>,
    reused: bool,
    pool_key: Option<String>,
    record: RecordPlan,
    handshake_turn_state: Option<String>,
}

/// How a streamed turn ended, and the continuation material captured along the way.
enum Outcome {
    Completed {
        response_id: Option<String>,
        output_items: Vec<Value>,
        turn_state: Option<String>,
    },
    /// The backend rejected the replayed `previous_response_id`.
    PreviousResponseMissing,
    /// Any non-clean end (error/incomplete terminal, close, transport error).
    Failed,
}

/// Reader task: stream the turn's events, then either record continuation state
/// and pool the connection (clean completion) or evict it (any failure). Holds the
/// connection lock for the whole turn, releasing it when the stream ends.
async fn run_reader(ctx: ReaderCtx, tx: mpsc::Sender<Result<ResponseEvent, CodexWsError>>) {
    let ReaderCtx {
        entry,
        mut guard,
        reused,
        pool_key,
        record,
        handshake_turn_state,
    } = ctx;
    let outcome = stream_events(&mut guard, &tx).await;
    match outcome {
        Outcome::Completed {
            response_id,
            output_items,
            turn_state,
        } => {
            if let Some(response_id) = response_id {
                let stored = StoredContinuation {
                    response_id,
                    signature: record.signature,
                    transcript: build_transcript(&record.request_input, &output_items),
                    turn_state: turn_state.or(handshake_turn_state),
                };
                *entry.continuation.lock().unwrap() = Some(stored);
            }
            *entry.last_used_at.lock().unwrap() = Instant::now();
            if let Some(key) = pool_key {
                // A pooled connection is already registered; a fresh one is added
                // now that it has proven healthy. Its continuation was updated above.
                if !reused {
                    pool_insert(key, entry.clone());
                }
            }
        }
        Outcome::PreviousResponseMissing | Outcome::Failed => {
            *entry.continuation.lock().unwrap() = None;
            if let Some(key) = pool_key {
                invalidate_pool_key(&key);
            }
        }
    }
    drop(guard); // release the connection for the next turn on this session
}

/// Pull frames until a terminal event, close, or error, forwarding each Text
/// frame as a [`ResponseEvent`] while capturing the response id, output items, and
/// turn-state token needed to record continuation.
async fn stream_events(
    stream: &mut WsStream,
    tx: &mpsc::Sender<Result<ResponseEvent, CodexWsError>>,
) -> Outcome {
    let mut response_id = None;
    let mut output_items = Vec::new();
    let mut turn_state = None;
    loop {
        let next = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
            Ok(next) => next,
            Err(_) => {
                let _ = tx
                    .send(Err(CodexWsError::transport(format!(
                        "websocket idle timeout after {}s",
                        IDLE_TIMEOUT.as_secs()
                    ))))
                    .await;
                return Outcome::Failed;
            }
        };

        match next {
            Some(Ok(Message::Text(text))) => {
                let Some(event) = parse_event(&text) else {
                    // Non-JSON or typeless frames carry no state the machine
                    // understands; skip them rather than aborting the stream.
                    tracing::debug!(frame = %text, "skipping unparseable codex ws frame");
                    continue;
                };
                // A rejected `previous_response_id` is not forwarded to the client;
                // it is signalled so the caller can retry with the full input.
                if is_previous_response_missing(&event.data) {
                    let _ = tx
                        .send(Err(CodexWsError::previous_response_missing()))
                        .await;
                    return Outcome::PreviousResponseMissing;
                }
                capture_continuation(&event, &mut response_id, &mut output_items, &mut turn_state);
                let name = event.event.as_deref().unwrap_or("");
                let is_terminal = TERMINAL_EVENTS.contains(&name);
                let completed = name == REUSABLE_TERMINAL;
                if tx.send(Ok(event)).await.is_err() {
                    return Outcome::Failed; // receiver dropped
                }
                if is_terminal {
                    return if completed {
                        Outcome::Completed {
                            response_id,
                            output_items,
                            turn_state,
                        }
                    } else {
                        Outcome::Failed
                    };
                }
            }
            Some(Ok(Message::Ping(data))) => {
                let _ = stream.send(Message::Pong(data)).await;
            }
            Some(Ok(Message::Binary(_))) => {
                let _ = tx
                    .send(Err(CodexWsError::transport(
                        "unexpected binary websocket frame",
                    )))
                    .await;
                return Outcome::Failed;
            }
            Some(Ok(Message::Close(frame))) => {
                // Closed before a terminal event: the turn was truncated, not
                // completed. Surface it as a transport error so the client sees an
                // Anthropic `error` event (or the JSON path logs a failure) rather
                // than a silently short, fake-success response.
                tracing::warn!(close_frame = ?frame, "codex websocket closed before a terminal event");
                let _ = tx
                    .send(Err(CodexWsError::transport(
                        "codex websocket closed before the response completed",
                    )))
                    .await;
                return Outcome::Failed;
            }
            None => {
                // Stream ended (EOF / dropped connection) before a terminal event —
                // same truncation case as an explicit Close.
                tracing::warn!("codex websocket stream ended before a terminal event");
                let _ = tx
                    .send(Err(CodexWsError::transport(
                        "codex websocket ended before the response completed",
                    )))
                    .await;
                return Outcome::Failed;
            }
            Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
            Some(Err(error)) => {
                let _ = tx
                    .send(Err(CodexWsError::transport(format!(
                        "websocket stream error: {error}"
                    ))))
                    .await;
                return Outcome::Failed;
            }
        }
    }
}

/// Capture the response id, output items, and turn-state token from a streamed
/// event for continuation. The response id appears on `response.created`/
/// `response.completed`; output items on `response.output_item.done`; the turn
/// state token may ride on any event body.
fn capture_continuation(
    event: &ResponseEvent,
    response_id: &mut Option<String>,
    output_items: &mut Vec<Value>,
    turn_state: &mut Option<String>,
) {
    // Only response-level events carry the response id; guard against picking up
    // an item id from e.g. `response.output_item.done`.
    let name = event.event.as_deref().unwrap_or("");
    if matches!(
        name,
        "response.created" | "response.in_progress" | "response.completed" | "response.done"
    ) {
        if let Some(id) = event
            .data
            .pointer("/response/id")
            .or_else(|| event.data.get("id"))
            .and_then(Value::as_str)
        {
            *response_id = Some(id.to_string());
        }
    }
    if event.event.as_deref() == Some("response.output_item.done") {
        if let Some(item) = event.data.get("item") {
            output_items.push(item.clone());
        }
    }
    if let Some(state) = event
        .data
        .get("turn_state")
        .or_else(|| event.data.pointer("/response/turn_state"))
        .and_then(Value::as_str)
    {
        *turn_state = Some(state.to_string());
    }
}

/// Whether an event reports the backend rejecting a replayed `previous_response_id`.
fn is_previous_response_missing(data: &Value) -> bool {
    if data
        .pointer("/error/code")
        .and_then(Value::as_str)
        .is_some_and(|code| code == "previous_response_not_found")
    {
        return true;
    }
    data.pointer("/error/message")
        .and_then(Value::as_str)
        .map(str::to_lowercase)
        .is_some_and(|message| {
            message.contains("previous response") && message.contains("not found")
        })
}

/// Parse a websocket text frame into a [`ResponseEvent`]. The Responses events
/// carry their SSE `event:` name in the JSON `type` field, so the machine can be
/// driven from it exactly as from the HTTP SSE stream.
fn parse_event(text: &str) -> Option<ResponseEvent> {
    let data: Value = serde_json::from_str(text).ok()?;
    let event = data.get("type").and_then(Value::as_str).map(str::to_string);
    event.as_ref()?; // typeless frames are not Responses events
    Some(ResponseEvent { event, data })
}

/// Map a tungstenite handshake failure to a [`CodexWsError`], extracting the HTTP
/// status, `retry-after`, and body when the upgrade was refused with a response.
fn map_handshake_error(error: tungstenite::Error) -> CodexWsError {
    if let tungstenite::Error::Http(response) = &error {
        let status = StatusCode::from_u16(response.status().as_u16()).ok();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = response
            .body()
            .as_ref()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default();
        return CodexWsError {
            status,
            retry_after,
            body,
            message: format!("websocket handshake rejected with {}", response.status()),
            previous_response_missing: false,
        };
    }
    CodexWsError::transport(format!("websocket connect error: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that touch the process-global connection [`POOL`]. Each
    /// clears the whole pool, so without this they would wipe each other's pooled
    /// entries mid-test when run in parallel.
    static POOL_TEST_LOCK: LazyLock<AsyncMutex<()>> = LazyLock::new(|| AsyncMutex::new(()));

    /// Convenience for the transport tests that don't exercise continuation:
    /// acquire a connection and stream a frame with no continuation recording.
    async fn open_simple(
        url: &str,
        headers: HeaderMap,
        frame: &Value,
        pool_key: Option<&str>,
    ) -> Result<CodexWsEvents, CodexWsError> {
        let turn = begin(url, headers, pool_key).await?;
        turn.stream(frame, RecordPlan::none()).await
    }

    #[test]
    fn rewrites_https_to_wss() {
        assert_eq!(
            to_websocket_url("https://chatgpt.com/backend-api/codex/responses").unwrap(),
            "wss://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            to_websocket_url("http://127.0.0.1:4141/codex/responses").unwrap(),
            "ws://127.0.0.1:4141/codex/responses"
        );
        assert_eq!(to_websocket_url("wss://host/x").unwrap(), "wss://host/x");
        assert!(to_websocket_url("ftp://host/x").is_err());
    }

    #[test]
    fn frame_carries_response_create_type() {
        let frame = response_create_frame(serde_json::json!({
            "model": "gpt-5.2-codex",
            "input": [],
            "stream": true
        }));
        assert_eq!(frame["type"], "response.create");
        // Existing fields are preserved alongside the tag.
        assert_eq!(frame["model"], "gpt-5.2-codex");
        assert_eq!(frame["stream"], true);
    }

    #[test]
    fn parse_event_reads_type_as_event_name() {
        let event = parse_event(r#"{"type":"response.output_text.delta","delta":"hi"}"#).unwrap();
        assert_eq!(event.event.as_deref(), Some("response.output_text.delta"));
        assert_eq!(event.data["delta"], "hi");
    }

    #[test]
    fn parse_event_rejects_typeless_and_non_json() {
        assert!(parse_event(r#"{"no_type":1}"#).is_none());
        assert!(parse_event("not json").is_none());
    }

    /// End-to-end over a real (loopback, plaintext) websocket: the transport must
    /// send a `response.create` frame and stream the backend's Responses events
    /// back in order, ending at the terminal event.
    #[tokio::test]
    async fn streams_response_events_end_to_end() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Mock backend: accept, read the client's frame, assert it, then emit a
        // minimal Responses event sequence and close.
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let Some(Ok(Message::Text(frame))) = ws.next().await else {
                panic!("expected a text frame from the client");
            };
            let frame: Value = serde_json::from_str(&frame).unwrap();
            assert_eq!(frame["type"], "response.create");
            assert_eq!(frame["model"], "gpt-5.2-codex");
            for event in [
                r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
                r#"{"type":"response.output_item.added","item":{"type":"message"}}"#,
                r#"{"type":"response.output_text.delta","delta":"hello"}"#,
                r#"{"type":"response.output_text.done"}"#,
                r#"{"type":"response.completed","response":{"usage":{"input_tokens":5,"output_tokens":2}}}"#,
            ] {
                ws.send(Message::Text(event.to_string())).await.unwrap();
            }
            ws.send(Message::Close(None)).await.unwrap();
        });

        let frame = response_create_frame(serde_json::json!({
            "model": "gpt-5.2-codex",
            "input": [],
            "stream": true,
        }));
        let mut events = open_simple(
            &format!("ws://{addr}/codex/responses"),
            HeaderMap::new(),
            &frame,
            None,
        )
        .await
        .expect("websocket should connect");

        // Drive the received events through the same machine the adapter uses.
        let mut machine =
            crate::model::responses::AnthropicSseMachine::new("gpt-5.2-codex", false, false);
        let mut names = Vec::new();
        let mut sse = String::new();
        while let Some(item) = events.recv().await {
            let event = item.expect("no transport error");
            names.push(event.event.clone().unwrap_or_default());
            sse.extend(machine.apply(event));
        }
        sse.extend(machine.finish());
        server.await.unwrap();

        assert_eq!(
            names,
            vec![
                "response.created",
                "response.output_item.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.completed",
            ]
        );
        assert!(sse.contains("message_start"), "sse: {sse}");
        assert!(sse.contains(r#""text":"hello""#), "sse: {sse}");
        assert!(sse.contains("message_stop"), "sse: {sse}");
    }

    /// A refused upgrade must surface the HTTP status and `retry-after` so the
    /// adapter can re-shape it exactly like an HTTP upstream error.
    #[tokio::test]
    async fn handshake_rejection_carries_status_and_retry_after() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0_u8; 1024];
            let _ = socket.read(&mut buffer).await;
            socket
                .write_all(
                    b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\nContent-Length: 0\r\n\r\n",
                )
                .await
                .unwrap();
        });

        let frame = response_create_frame(serde_json::json!({"model": "m", "input": []}));
        let error = open_simple(
            &format!("ws://{addr}/codex/responses"),
            HeaderMap::new(),
            &frame,
            None,
        )
        .await
        .expect_err("handshake should be refused");
        assert_eq!(error.status, Some(StatusCode::TOO_MANY_REQUESTS));
        assert_eq!(error.retry_after.as_deref(), Some("7"));
    }

    /// A `response.completed` turn pools its connection under the session key, and
    /// the next turn on that session reuses the same socket. The mock server
    /// accepts exactly once, so a passing second turn proves reuse (a fresh
    /// handshake would find no listener).
    #[tokio::test]
    async fn pooled_connection_is_reused_across_turns() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::net::TcpListener;

        let _pool_guard = POOL_TEST_LOCK.lock().await;
        clear_pool_for_tests();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let frames_seen = Arc::new(AtomicUsize::new(0));
        let server_frames = frames_seen.clone();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            // One connection, many turns: respond to each response.create frame
            // with a complete event sequence; ignore the liveness ping.
            while let Some(message) = ws.next().await {
                match message.unwrap() {
                    Message::Text(frame) => {
                        let frame: Value = serde_json::from_str(&frame).unwrap();
                        assert_eq!(frame["type"], "response.create");
                        server_frames.fetch_add(1, Ordering::SeqCst);
                        for event in [
                            r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
                            r#"{"type":"response.output_text.delta","delta":"hi"}"#,
                            r#"{"type":"response.completed","response":{}}"#,
                        ] {
                            ws.send(Message::Text(event.to_string())).await.unwrap();
                        }
                    }
                    Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(_) => break,
                    other => panic!("unexpected frame: {other:?}"),
                }
            }
        });

        let url = format!("ws://{addr}/codex/responses");
        let frame = response_create_frame(serde_json::json!({"model": "m", "input": []}));

        // Turn 1: fresh connection, drained to completion, then pooled.
        let mut turn1 = open_simple(&url, HeaderMap::new(), &frame, Some("session-1"))
            .await
            .expect("first turn connects");
        drain(&mut turn1).await;
        assert!(
            pool_contains_for_tests("session-1"),
            "completed turn pools its connection"
        );

        // Turn 2: reuses the pooled socket (the mock only accepts once).
        let mut turn2 = open_simple(&url, HeaderMap::new(), &frame, Some("session-1"))
            .await
            .expect("second turn reuses connection");
        let count = drain(&mut turn2).await;
        assert!(
            count > 0,
            "second turn streamed events over the reused socket"
        );
        assert_eq!(
            frames_seen.load(Ordering::SeqCst),
            2,
            "both turns reached the single mock connection"
        );

        clear_pool_for_tests();
        server.abort();
    }

    /// A socket that closes before a terminal event must surface a transport error
    /// on the channel — not end quietly — so the client sees a truncation failure
    /// instead of a silently short, fake-success response.
    #[tokio::test]
    async fn close_before_terminal_event_surfaces_error() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let Some(Ok(Message::Text(_))) = ws.next().await else {
                panic!("expected a client frame");
            };
            // One non-terminal event, then close WITHOUT a terminal event.
            ws.send(Message::Text(
                r#"{"type":"response.created","response":{"id":"resp_1"}}"#.to_string(),
            ))
            .await
            .unwrap();
            ws.send(Message::Close(None)).await.unwrap();
        });

        let frame = response_create_frame(serde_json::json!({"model": "m", "input": []}));
        let mut events = open_simple(
            &format!("ws://{addr}/codex/responses"),
            HeaderMap::new(),
            &frame,
            None,
        )
        .await
        .expect("websocket should connect");

        let mut saw_created = false;
        let mut saw_error = false;
        while let Some(item) = events.recv().await {
            match item {
                Ok(event) => {
                    if event.event.as_deref() == Some("response.created") {
                        saw_created = true;
                    }
                }
                Err(error) => {
                    assert!(
                        error.message.contains("closed") || error.message.contains("ended"),
                        "unexpected error: {}",
                        error.message
                    );
                    saw_error = true;
                }
            }
        }
        assert!(
            saw_created,
            "the pre-close event was forwarded to the client"
        );
        assert!(
            saw_error,
            "a close before a terminal event surfaced a transport error"
        );
        server.await.unwrap();
    }

    /// Drain a receiver to exhaustion, returning how many `Ok` events arrived.
    async fn drain(events: &mut CodexWsEvents) -> usize {
        let mut count = 0;
        while let Some(item) = events.recv().await {
            if item.is_ok() {
                count += 1;
            }
        }
        count
    }

    /// A completed turn captures the response id and its output items as
    /// continuation state on the pooled connection, so the next turn on that
    /// session can reuse `previous_response_id`. The stored transcript is the
    /// turn's logical input followed by the backend's `output_item.done` items.
    #[tokio::test]
    async fn completed_turn_records_continuation_for_reuse() {
        use tokio::net::TcpListener;

        let _pool_guard = POOL_TEST_LOCK.lock().await;
        clear_pool_for_tests();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            while let Some(message) = ws.next().await {
                match message.unwrap() {
                    Message::Text(_) => {
                        for event in [
                            r#"{"type":"response.created","response":{"id":"resp_cont_1"}}"#,
                            r#"{"type":"response.output_item.done","item":{"type":"message","role":"assistant","id":"msg_1","phase":"final_answer","status":"completed","content":[{"type":"output_text","text":"hello","annotations":[],"logprobs":[]}]}}"#,
                            r#"{"type":"response.completed","response":{"id":"resp_cont_1"}}"#,
                        ] {
                            ws.send(Message::Text(event.to_string())).await.unwrap();
                        }
                    }
                    Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(_) => break,
                    other => panic!("unexpected frame: {other:?}"),
                }
            }
        });

        let url = format!("ws://{addr}/codex/responses");
        let frame = response_create_frame(serde_json::json!({"model": "m", "input": []}));
        let user_hi = serde_json::json!({
            "type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "hi"}]
        });

        // Turn 1: record continuation from a real completion.
        let turn1 = begin(&url, HeaderMap::new(), Some("sess-cont"))
            .await
            .expect("first turn connects");
        let mut events = turn1
            .stream(
                &frame,
                RecordPlan {
                    signature: "sig-a".to_string(),
                    request_input: vec![user_hi.clone()],
                },
            )
            .await
            .expect("first turn streams");
        drain(&mut events).await;

        // Turn 2: the reused connection exposes the stored continuation.
        let turn2 = begin(&url, HeaderMap::new(), Some("sess-cont"))
            .await
            .expect("second turn reuses connection");
        let stored = turn2
            .stored_continuation()
            .expect("completed turn records continuation on the reused connection");
        assert_eq!(stored.response_id, "resp_cont_1");
        assert_eq!(stored.signature, "sig-a");
        assert_eq!(stored.transcript.len(), 2, "input ++ one output item");
        assert_eq!(stored.transcript[0], user_hi);
        assert_eq!(stored.transcript[1]["role"], "assistant");
        assert_eq!(stored.transcript[1]["id"], "msg_1");
        drop(turn2); // release the connection without streaming

        clear_pool_for_tests();
        server.abort();
    }

    /// A backend `previous_response_not_found` is not forwarded as a normal event:
    /// the receiver gets a flagged transport error the adapter can retry on, and the
    /// connection is evicted (its server-side context is gone).
    #[tokio::test]
    async fn previous_response_missing_signals_and_invalidates() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::net::TcpListener;

        let _pool_guard = POOL_TEST_LOCK.lock().await;
        clear_pool_for_tests();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let frames = Arc::new(AtomicUsize::new(0));
        let server_frames = frames.clone();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            while let Some(message) = ws.next().await {
                match message.unwrap() {
                    Message::Text(_) => {
                        // First turn completes (and pools); the second is rejected.
                        let n = server_frames.fetch_add(1, Ordering::SeqCst);
                        let events: &[&str] = if n == 0 {
                            &[
                                r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
                                r#"{"type":"response.completed","response":{"id":"resp_1"}}"#,
                            ]
                        } else {
                            &[
                                r#"{"type":"error","error":{"code":"previous_response_not_found","message":"Previous response not found"}}"#,
                            ]
                        };
                        for event in events {
                            ws.send(Message::Text(event.to_string())).await.unwrap();
                        }
                    }
                    Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(_) => break,
                    other => panic!("unexpected frame: {other:?}"),
                }
            }
        });

        let url = format!("ws://{addr}/codex/responses");
        let frame = response_create_frame(serde_json::json!({"model": "m", "input": []}));

        // Turn 1: complete + pool.
        let mut turn1 = open_simple(&url, HeaderMap::new(), &frame, Some("sess-miss"))
            .await
            .expect("first turn connects");
        drain(&mut turn1).await;
        assert!(pool_contains_for_tests("sess-miss"), "first turn pools");

        // Turn 2: reused, but the backend rejects the replayed previous_response_id.
        let mut turn2 = open_simple(&url, HeaderMap::new(), &frame, Some("sess-miss"))
            .await
            .expect("second turn reuses connection");
        let mut saw_missing = false;
        while let Some(item) = turn2.recv().await {
            if let Err(error) = item {
                assert!(error.previous_response_missing);
                saw_missing = true;
            }
        }
        assert!(saw_missing, "receiver observes the flagged rejection");
        assert!(
            !pool_contains_for_tests("sess-miss"),
            "a rejected continuation evicts the connection"
        );

        clear_pool_for_tests();
        server.abort();
    }
}
