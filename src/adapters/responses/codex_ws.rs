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
//! conversation reuse a live socket instead of re-handshaking. Each pooled
//! socket is owned by a dedicated reader task that lives for the whole
//! connection (issue #93): it answers upstream `Ping` frames with `Pong` even
//! while the connection sits idle between turns, so the backend never closes it
//! with `keepalive ping timeout`. A turn is dispatched to that reader over a
//! command channel; the reader streams the turn's events, records continuation
//! state on a clean completion, and returns to idle keepalive duty. Because the
//! reader forwards events over an *unbounded* channel it never blocks on
//! downstream backpressure, so control frames are always serviced promptly.
//!
//! On a reused connection this module also records the completed turn's response
//! id and output items as [`StoredContinuation`], so the next turn can replay
//! `previous_response_id` and upload only the input delta (the decision itself
//! lives in [`crate::adapters::responses::codex_continuation`]). `previous_response_id` is
//! only ever valid on the exact connection that produced it, which is why the
//! continuation state is stored on the [`Connection`] rather than globally.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, StatusCode};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex as AsyncMutex, Notify, OwnedMutexGuard};
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
/// Idle ceiling between frames *during an active turn*. Reset on every frame
/// (including keepalive pings), so a healthy but slow generation never trips it.
/// A pooled connection between turns is not bounded by this — its reader waits
/// indefinitely for the next turn while answering pings, and pool TTL handles
/// eviction.
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// How long a reuse liveness probe waits for the backend's `Pong` before the
/// pooled connection is judged stale and replaced. A healthy socket answers in
/// well under a round-trip; only a half-open one pays the full wait.
const REUSE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
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
/// Write half of a split [`WsStream`]. Shared between the reader (for `Pong`
/// replies and sending a turn's `response.create`) and the reuse liveness probe.
type WsSink = SplitSink<WsStream, Message>;
/// Read half of a split [`WsStream`], owned solely by the connection's reader task.
type WsSource = SplitStream<WsStream>;

/// A turn dispatched to a connection's reader task: the frame to send, where to
/// stream events, what to record for continuation, and the turn slot to release
/// when the turn ends.
struct StartTurn {
    /// The `response.create` text frame to write before streaming.
    frame: Message,
    /// Where the reader forwards this turn's events. Unbounded so downstream
    /// backpressure never blocks the reader (and thus never starves `Pong`s).
    events: mpsc::UnboundedSender<Result<ResponseEvent, CodexWsError>>,
    /// What to record as continuation state on a clean completion.
    record: RecordPlan,
    /// Held for the turn's duration; dropped by the reader when the turn ends,
    /// freeing the connection for the next turn on this session.
    slot: OwnedMutexGuard<()>,
}

/// A pooled websocket connection and everything shared between its reader task,
/// the turns that stream over it, and the reuse liveness probe. The read half is
/// owned exclusively by the reader task; the write half (`sink`) is shared.
struct Connection {
    /// Write half, guarded so the reader (`Pong`), a turn (`response.create`),
    /// and a probe (`Ping`) can each write with a short critical section.
    sink: AsyncMutex<WsSink>,
    /// Serializes turns: at most one `response.create` is in flight at a time.
    /// The owning guard is handed to the reader for the turn and released when
    /// the turn ends. Behind an [`Arc`] so `begin` can `lock_owned`.
    turn_lock: Arc<AsyncMutex<()>>,
    /// Cleared once the reader observes a close/EOF/stream error, so a reuse
    /// probe rejects a known-dead socket without a round-trip.
    alive: AtomicBool,
    /// Notified by the reader whenever a `Pong` arrives, so the reuse probe can
    /// wait for definitive remote liveness instead of trusting a local write.
    pong: Notify,
    /// Notified when the pooled entry is evicted, so the reader shuts down
    /// instead of holding the socket and its task open forever.
    shutdown: Notify,
    /// Turn dispatch channel to the reader (bounded at 1: `turn_lock` guarantees
    /// at most one outstanding turn).
    commands: mpsc::Sender<StartTurn>,
    /// Continuation captured from this connection's last completed turn.
    continuation: Mutex<Option<StoredContinuation>>,
    /// Last time a turn used this connection; drives idle TTL eviction.
    last_used_at: Mutex<Instant>,
    /// The session key this connection is pooled under, if any.
    pool_key: Option<String>,
    /// The `x-codex-turn-state` captured from the handshake, if present.
    handshake_turn_state: Option<String>,
}

impl Connection {
    /// Perform the handshake, split the socket, and spawn the connection-owned
    /// reader task. The returned handle shares state with that task.
    async fn open(
        ws_url: &str,
        headers: HeaderMap,
        pool_key: Option<String>,
    ) -> Result<Arc<Self>, CodexWsError> {
        let (stream, handshake_turn_state) = connect(ws_url, headers).await?;
        let (sink, source) = stream.split();
        let (command_tx, command_rx) = mpsc::channel(1);
        let conn = Arc::new(Self {
            sink: AsyncMutex::new(sink),
            turn_lock: Arc::new(AsyncMutex::new(())),
            alive: AtomicBool::new(true),
            pong: Notify::new(),
            shutdown: Notify::new(),
            commands: command_tx,
            continuation: Mutex::new(None),
            last_used_at: Mutex::new(Instant::now()),
            pool_key,
            handshake_turn_state,
        });
        tokio::spawn(run_connection(conn.clone(), source, command_rx));
        Ok(conn)
    }
}

/// A pooled connection keyed by session id in [`POOL`]. Thin wrapper whose
/// `Drop` tells the reader to shut down: when the last reference goes (TTL sweep,
/// capacity eviction, or `invalidate_pool_key`) the socket and its task are
/// released. The reader itself holds an [`Arc<Connection>`], not a `PoolEntry`,
/// so removing an entry from the map is what triggers the shutdown.
struct PoolEntry {
    conn: Arc<Connection>,
}

impl PoolEntry {
    fn new(conn: Arc<Connection>) -> Arc<Self> {
        Arc::new(Self { conn })
    }
}

impl Drop for PoolEntry {
    fn drop(&mut self) {
        // The last pooled reference is gone: wake the reader so it stops keeping
        // the idle socket alive. `notify_one` stores a permit if the reader is
        // momentarily not awaiting, so the signal is never lost.
        self.conn.shutdown.notify_one();
    }
}

/// Process-global connection pool keyed by `x-claude-code-session-id`. A std
/// mutex guards only map lookups/inserts (never held across an await); the
/// per-connection reader task serializes turns on one session.
static POOL: LazyLock<Mutex<HashMap<String, Arc<PoolEntry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static LAST_POOL_SWEEP: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));

fn pool_get(key: &str) -> Option<Arc<PoolEntry>> {
    POOL.lock().unwrap().get(key).cloned()
}

/// Remove a session's pooled connection (called on staleness or any error).
pub fn invalidate_pool_key(key: &str) {
    POOL.lock().unwrap().remove(key);
}

fn pool_insert(key: String, entry: Arc<PoolEntry>) {
    let mut guard = POOL.lock().unwrap();
    let mut last_sweep = LAST_POOL_SWEEP.lock().unwrap();
    let sweep_due = last_sweep.elapsed() >= POOL_SWEEP_INTERVAL;
    if sweep_due || guard.len() >= MAX_POOL_ENTRIES {
        // Sweep at most once per interval during ordinary churn, but always sweep
        // under capacity pressure before choosing an LRU victim.
        guard.retain(|_, entry| entry.conn.last_used_at.lock().unwrap().elapsed() < POOL_IDLE_TTL);
        *last_sweep = Instant::now();
    }
    drop(last_sweep);
    if guard.len() >= MAX_POOL_ENTRIES {
        // Evict the least recently used connection. `HashMap` iteration order is
        // unspecified, so `keys().next()` would drop an arbitrary (possibly active)
        // entry instead of the stalest one.
        if let Some(oldest) = guard
            .iter()
            .min_by_key(|(_, entry)| *entry.conn.last_used_at.lock().unwrap())
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
/// the stream (the reader stops the turn after sending it). Unbounded so the
/// reader never blocks forwarding events, keeping control-frame handling
/// independent of downstream consumption speed.
pub type CodexWsEvents = mpsc::UnboundedReceiver<Result<ResponseEvent, CodexWsError>>;

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
/// delta before committing the frame. The held turn slot serializes turns on the
/// connection until [`Turn::stream`] hands it to the reader (or the `Turn` is
/// dropped without streaming).
pub struct Turn {
    conn: Arc<Connection>,
    /// The held turn slot, taken by [`Turn::stream`] when it dispatches the turn to
    /// the reader. Wrapped in `Option` so `stream` can move it out even though
    /// `Turn` has a `Drop` impl (a field cannot be moved out of a `Drop` type).
    slot: Option<OwnedMutexGuard<()>>,
    reused: bool,
    pool_key: Option<String>,
    /// Set by [`Turn::stream`] once the turn is handed to the reader. A fresh
    /// connection whose `Turn` is dropped before this is set has no `PoolEntry` to
    /// fire `shutdown`, so [`Turn`]'s `Drop` wakes the reader itself to avoid
    /// leaking the reader task and its socket.
    streamed: bool,
}

impl Turn {
    /// The continuation state captured on this connection's previous turn.
    /// `None` for a fresh connection — `previous_response_id` is only valid on the
    /// connection that produced it.
    pub fn stored_continuation(&self) -> Option<StoredContinuation> {
        if !self.reused {
            return None;
        }
        self.conn.continuation.lock().unwrap().clone()
    }

    /// The `x-codex-turn-state` captured from the handshake, if any.
    pub fn handshake_turn_state(&self) -> Option<&str> {
        self.conn.handshake_turn_state.as_deref()
    }

    /// Dispatch the `response.create` frame to the connection's reader and stream
    /// events back. The reader sends the frame, streams the turn, records new
    /// continuation state on a clean completion, and returns the connection to
    /// idle keepalive duty (or evicts it on any failure).
    pub async fn stream(
        mut self,
        frame: &Value,
        record: RecordPlan,
    ) -> Result<CodexWsEvents, CodexWsError> {
        let payload = serde_json::to_string(frame).map_err(|error| {
            CodexWsError::transport(format!("failed to encode ws frame: {error}"))
        })?;
        // Past the last fallible step before dispatch: mark the turn streamed so
        // `Drop` does not also signal shutdown for the connection we are about to
        // hand to the reader, then take the slot to move it into the command.
        self.streamed = true;
        let slot = self
            .slot
            .take()
            .expect("turn slot is present until the turn is streamed");
        let conn = self.conn.clone();
        let reused = self.reused;
        let pool_key = self.pool_key.take();
        let (tx, rx) = mpsc::unbounded_channel();
        let command = StartTurn {
            frame: Message::Text(payload),
            events: tx,
            record,
            slot,
        };
        if conn.commands.send(command).await.is_err() {
            // The connection-owned reader is gone (socket dead). Evict a reused
            // entry so the next turn on this session opens a fresh socket instead
            // of re-probing the same dead one.
            if reused {
                if let Some(key) = &pool_key {
                    invalidate_pool_key(key);
                }
            }
            return Err(CodexWsError::transport("codex websocket reader is gone"));
        }
        Ok(rx)
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        // A fresh connection whose `Turn` is abandoned before `stream` dispatched it
        // has no pooled `PoolEntry` to fire `shutdown` on drop, and the reader holds
        // the only `Arc<Connection>` (so its `commands.recv()` never ends). Without a
        // nudge the reader task and its socket would leak, so wake it to exit. A
        // reused connection is already pooled — its `PoolEntry` owns shutdown — and a
        // streamed turn is owned by the reader, so both are left untouched.
        if !self.reused && !self.streamed {
            self.conn.shutdown.notify_one();
        }
    }
}

/// Acquire a connection for `pool_key`, reusing a live pooled one (verified with a
/// `Ping`/`Pong` liveness probe) or performing a fresh handshake. A stale pooled
/// connection is evicted and replaced. A refused handshake (401/403/429) resolves
/// to `Err` with the upstream status/body so the caller can re-shape it like the
/// HTTP path.
pub async fn begin(
    ws_url: &str,
    headers: HeaderMap,
    pool_key: Option<&str>,
) -> Result<Turn, CodexWsError> {
    if let Some(key) = pool_key {
        if let Some(entry) = pool_get(key) {
            let conn = entry.conn.clone();
            // Serialize turns on this connection; holding the slot also guarantees
            // no turn is streaming while we probe. The connection-owned reader
            // keeps reading (and answering pings) independently, so the probe's
            // Ping/Pong round-trips underneath it.
            let slot = conn.turn_lock.clone().lock_owned().await;
            if conn.alive.load(Ordering::SeqCst) && probe_live(&conn).await {
                *conn.last_used_at.lock().unwrap() = Instant::now();
                return Ok(Turn {
                    conn,
                    slot: Some(slot),
                    reused: true,
                    pool_key: Some(key.to_string()),
                    streamed: false,
                });
            }
            // Stale: the reader saw a close, or no Pong returned in time. Evict and
            // reconnect — the stored `previous_response_id` no longer applies.
            drop(slot);
            drop(entry);
            invalidate_pool_key(key);
        }
    }

    let conn = Connection::open(ws_url, headers, pool_key.map(str::to_string)).await?;
    let slot = conn.turn_lock.clone().lock_owned().await;
    Ok(Turn {
        conn,
        slot: Some(slot),
        reused: false,
        pool_key: pool_key.map(str::to_string),
        streamed: false,
    })
}

/// Confirm a pooled connection is still live by requiring a timely `Pong` from
/// the backend, not merely a successful local `Ping` write — a half-open socket
/// buffers the write and would otherwise pass. The connection-owned reader
/// observes the `Pong` and wakes this waiter.
async fn probe_live(conn: &Connection) -> bool {
    let notified = conn.pong.notified();
    tokio::pin!(notified);
    // Register interest *before* sending, so a Pong that races back is not missed.
    notified.as_mut().enable();
    {
        let mut sink = conn.sink.lock().await;
        if sink.send(Message::Ping(Vec::new())).await.is_err() {
            return false;
        }
    }
    tokio::time::timeout(REUSE_PROBE_TIMEOUT, notified)
        .await
        .is_ok()
}

/// Write a single frame through the connection's shared sink.
async fn send_message(conn: &Connection, message: Message) -> Result<(), tungstenite::Error> {
    conn.sink.lock().await.send(message).await
}

/// Drop this connection's pooled entry (if any), so the next turn on the session
/// opens a fresh socket. Called from the reader on any non-clean turn end.
fn evict(conn: &Connection) {
    if let Some(key) = &conn.pool_key {
        invalidate_pool_key(key);
    }
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

/// How a streamed turn ended.
enum TurnEnd {
    /// The turn completed cleanly; the connection is healthy and stays pooled.
    Completed,
    /// Any non-clean end (error/incomplete terminal, `previous_response_not_found`,
    /// close, EOF, transport error). The connection is evicted and closed.
    Dead,
}

/// Surface an explicit error on any turn command still buffered when the reader
/// exits, instead of letting it drop silently. A turn can be dispatched during the
/// reader's teardown: `begin` returns before a `Close`/EOF breaks the loop, then
/// `stream` sends into the (capacity-1, not-yet-dropped) channel and gets `Ok`, but
/// the reader never receives it — the caller's event stream would just end with a
/// bare `None`, read as an empty "ghost" response. Closing the channel first makes a
/// concurrent `stream` fail fast (the "reader is gone" path) rather than buffer, and
/// draining what is already buffered turns it into a proper transport error.
fn fail_pending_commands(commands: &mut mpsc::Receiver<StartTurn>) {
    commands.close();
    while let Ok(StartTurn { events, .. }) = commands.try_recv() {
        let _ = events.send(Err(CodexWsError::transport(
            "codex websocket closed before the turn could start",
        )));
    }
}

/// Connection-owned reader task: owns the read half for the socket's lifetime.
/// While idle it answers upstream `Ping` frames (so the backend never times the
/// connection out) and watches for a dispatched turn or a shutdown signal. While
/// a turn is active it streams that turn's events, records continuation, and
/// returns to idle. Exits — closing the socket — on any close/EOF/error or when
/// the pooled entry is evicted.
async fn run_connection(
    conn: Arc<Connection>,
    mut source: WsSource,
    mut commands: mpsc::Receiver<StartTurn>,
) {
    let mut pooled = false;
    loop {
        tokio::select! {
            biased;
            _ = conn.shutdown.notified() => break,
            command = commands.recv() => {
                let Some(StartTurn { frame, events, record, slot }) = command else {
                    // All turn senders dropped — the connection handle is gone.
                    break;
                };
                if let Err(error) = send_message(&conn, frame).await {
                    let _ = events.send(Err(CodexWsError::transport(format!(
                        "websocket send failed: {error}"
                    ))));
                    evict(&conn);
                    drop(events);
                    drop(slot);
                    break;
                }
                let end = run_turn(&conn, &mut source, &events, record, &mut pooled).await;
                drop(events); // end the client's stream (receiver observes None)
                drop(slot); // release the turn slot for the next turn
                if matches!(end, TurnEnd::Dead) || conn.pool_key.is_none() {
                    // A non-pooled connection (no session key) is used for exactly
                    // one turn — it is never registered for reuse — so once that
                    // turn ends there is nothing left to serve. Exit instead of
                    // idling forever answering pings, which would leak the reader
                    // task and its socket for every session-less request.
                    break;
                }
            }
            frame = source.next() => {
                match frame {
                    Some(Ok(Message::Ping(data))) => {
                        let _ = send_message(&conn, Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Pong(_))) => conn.pong.notify_waiters(),
                    Some(Ok(Message::Text(text))) => {
                        // No turn is active; the backend should not send data here.
                        tracing::debug!(frame = %text, "discarding codex ws frame received while idle");
                    }
                    Some(Ok(Message::Binary(_))) | Some(Ok(Message::Frame(_))) => {}
                    Some(Ok(Message::Close(frame))) => {
                        tracing::debug!(close_frame = ?frame, "codex websocket closed while idle in pool");
                        break;
                    }
                    None => {
                        tracing::debug!("codex websocket ended while idle in pool");
                        break;
                    }
                    Some(Err(error)) => {
                        tracing::debug!(%error, "codex websocket error while idle in pool");
                        break;
                    }
                }
            }
        }
    }

    // Before anything else: fail (and stop accepting) any turn command that raced
    // the reader's exit, so a turn dispatched during teardown surfaces an error
    // instead of a silent empty stream.
    fail_pending_commands(&mut commands);
    conn.alive.store(false, Ordering::SeqCst);
    // Catch-all eviction for every exit reason (shutdown, close/EOF/error while
    // idle, commands channel exhausted, send failure). The `TurnEnd::Dead` branches
    // in `run_turn` also evict, deliberately: they run *before* the turn slot is
    // released above, so a concurrent `begin` waiting on `turn_lock` can never reuse
    // a dying connection. `invalidate_pool_key` is idempotent, so the overlap on the
    // Dead path is harmless.
    evict(&conn);
    // Dropping `source` closes the read half; best-effort close the write half so
    // the backend sees a clean shutdown.
    let mut sink = conn.sink.lock().await;
    let _ = sink.close().await;
}

/// Stream one turn: pull frames until a terminal event, close, or error,
/// forwarding each Text frame as a [`ResponseEvent`] while capturing the response
/// id, output items, and turn-state token needed to record continuation. Answers
/// `Ping` frames inline (independent of the unbounded event channel, so downstream
/// backpressure never starves control-frame handling). On a clean completion the
/// continuation is recorded and, for a not-yet-pooled connection, the connection
/// is pooled.
async fn run_turn(
    conn: &Arc<Connection>,
    source: &mut WsSource,
    events: &mpsc::UnboundedSender<Result<ResponseEvent, CodexWsError>>,
    record: RecordPlan,
    pooled: &mut bool,
) -> TurnEnd {
    let mut response_id = None;
    let mut output_items = Vec::new();
    let mut turn_state = None;
    loop {
        let next = match tokio::time::timeout(IDLE_TIMEOUT, source.next()).await {
            Ok(next) => next,
            Err(_) => {
                let _ = events.send(Err(CodexWsError::transport(format!(
                    "websocket idle timeout after {}s",
                    IDLE_TIMEOUT.as_secs()
                ))));
                evict(conn);
                return TurnEnd::Dead;
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
                    let _ = events.send(Err(CodexWsError::previous_response_missing()));
                    *conn.continuation.lock().unwrap() = None;
                    evict(conn);
                    return TurnEnd::Dead;
                }
                capture_continuation(&event, &mut response_id, &mut output_items, &mut turn_state);
                let name = event.event.as_deref().unwrap_or("");
                let is_terminal = TERMINAL_EVENTS.contains(&name);
                let completed = name == REUSABLE_TERMINAL;
                if events.send(Ok(event)).is_err() {
                    // Receiver dropped (client cancelled): the turn is abandoned.
                    evict(conn);
                    return TurnEnd::Dead;
                }
                if is_terminal {
                    if completed {
                        if let Some(response_id) = response_id {
                            let stored = StoredContinuation {
                                response_id,
                                signature: record.signature,
                                transcript: build_transcript(&record.request_input, &output_items),
                                turn_state: turn_state
                                    .or_else(|| conn.handshake_turn_state.clone()),
                            };
                            *conn.continuation.lock().unwrap() = Some(stored);
                        }
                        *conn.last_used_at.lock().unwrap() = Instant::now();
                        if !*pooled {
                            if let Some(key) = &conn.pool_key {
                                // First clean completion on a fresh connection: it
                                // has proven healthy, so register it for reuse.
                                pool_insert(key.clone(), PoolEntry::new(conn.clone()));
                                *pooled = true;
                            }
                        }
                        return TurnEnd::Completed;
                    }
                    *conn.continuation.lock().unwrap() = None;
                    evict(conn);
                    return TurnEnd::Dead;
                }
            }
            Some(Ok(Message::Ping(data))) => {
                let _ = send_message(conn, Message::Pong(data)).await;
            }
            Some(Ok(Message::Pong(_))) => conn.pong.notify_waiters(),
            Some(Ok(Message::Binary(_))) => {
                let _ = events.send(Err(CodexWsError::transport(
                    "unexpected binary websocket frame",
                )));
                evict(conn);
                return TurnEnd::Dead;
            }
            Some(Ok(Message::Close(frame))) => {
                // Closed before a terminal event: the turn was truncated, not
                // completed. Surface it as a transport error so the client sees an
                // Anthropic `error` event (or the JSON path logs a failure) rather
                // than a silently short, fake-success response.
                tracing::warn!(close_frame = ?frame, "codex websocket closed before a terminal event");
                let _ = events.send(Err(CodexWsError::transport(
                    "codex websocket closed before the response completed",
                )));
                evict(conn);
                return TurnEnd::Dead;
            }
            None => {
                // Stream ended (EOF / dropped connection) before a terminal event —
                // same truncation case as an explicit Close.
                tracing::warn!("codex websocket stream ended before a terminal event");
                let _ = events.send(Err(CodexWsError::transport(
                    "codex websocket ended before the response completed",
                )));
                evict(conn);
                return TurnEnd::Dead;
            }
            Some(Ok(Message::Frame(_))) => {}
            Some(Err(error)) => {
                let _ = events.send(Err(CodexWsError::transport(format!(
                    "websocket stream error: {error}"
                ))));
                evict(conn);
                return TurnEnd::Dead;
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
            // with a complete event sequence; answer the reuse liveness Ping.
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
                    Message::Ping(data) => ws.send(Message::Pong(data)).await.unwrap(),
                    Message::Pong(_) => {}
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
                    Message::Ping(data) => ws.send(Message::Pong(data)).await.unwrap(),
                    Message::Pong(_) => {}
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
                    Message::Ping(data) => ws.send(Message::Pong(data)).await.unwrap(),
                    Message::Pong(_) => {}
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

    /// Issue #93: a pooled connection sitting idle between turns must keep
    /// answering upstream `Ping` frames, so the backend never closes it with
    /// `keepalive ping timeout`. The connection-owned reader answers even though
    /// no turn is streaming.
    #[tokio::test]
    async fn idle_pooled_socket_answers_ping() {
        use tokio::net::TcpListener;

        let _pool_guard = POOL_TEST_LOCK.lock().await;
        clear_pool_for_tests();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            // Turn 1: complete so the connection is pooled and goes idle.
            let Some(Ok(Message::Text(_))) = ws.next().await else {
                panic!("expected a client frame");
            };
            for event in [
                r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
                r#"{"type":"response.completed","response":{"id":"resp_1"}}"#,
            ] {
                ws.send(Message::Text(event.to_string())).await.unwrap();
            }
            // Now idle in the pool: send a keepalive Ping and require a Pong back.
            ws.send(Message::Ping(b"ka".to_vec())).await.unwrap();
            loop {
                match ws.next().await {
                    Some(Ok(Message::Pong(data))) => {
                        assert_eq!(data, b"ka");
                        break;
                    }
                    Some(Ok(_)) => continue,
                    other => panic!("expected a Pong while idle, got {other:?}"),
                }
            }
        });

        let url = format!("ws://{addr}/codex/responses");
        let frame = response_create_frame(serde_json::json!({"model": "m", "input": []}));
        let mut turn1 = open_simple(&url, HeaderMap::new(), &frame, Some("idle-ka"))
            .await
            .expect("first turn connects");
        drain(&mut turn1).await;
        assert!(pool_contains_for_tests("idle-ka"), "turn pools its socket");

        // The pooled reader must answer the server's idle Ping with a Pong.
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server should observe a Pong while the connection is idle")
            .unwrap();

        clear_pool_for_tests();
    }

    /// Issue #93: a pooled connection the backend has closed (e.g. after a
    /// `keepalive ping timeout`) must be evicted and replaced with a fresh
    /// handshake before the next turn streams — never reused into a mid-stream
    /// failure.
    #[tokio::test]
    async fn stale_pooled_connection_is_replaced_before_new_turn() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use tokio::net::TcpListener;

        let _pool_guard = POOL_TEST_LOCK.lock().await;
        clear_pool_for_tests();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepts = Arc::new(AtomicUsize::new(0));
        let server_accepts = accepts.clone();
        let server = tokio::spawn(async move {
            // Connection 1: complete turn 1, then close while it sits idle in the
            // pool (as the backend does on keepalive ping timeout).
            let (socket, _) = listener.accept().await.unwrap();
            server_accepts.fetch_add(1, Ordering::SeqCst);
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let Some(Ok(Message::Text(_))) = ws.next().await else {
                panic!("expected a client frame");
            };
            for event in [
                r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
                r#"{"type":"response.completed","response":{"id":"resp_1"}}"#,
            ] {
                ws.send(Message::Text(event.to_string())).await.unwrap();
            }
            ws.send(Message::Close(None)).await.unwrap();
            drop(ws);

            // Connection 2: turn 2 must open a fresh handshake here; complete it.
            let (socket, _) = listener.accept().await.unwrap();
            server_accepts.fetch_add(1, Ordering::SeqCst);
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            while let Some(message) = ws.next().await {
                match message.unwrap() {
                    Message::Text(_) => {
                        for event in [
                            r#"{"type":"response.created","response":{"id":"resp_2"}}"#,
                            r#"{"type":"response.completed","response":{"id":"resp_2"}}"#,
                        ] {
                            ws.send(Message::Text(event.to_string())).await.unwrap();
                        }
                        break;
                    }
                    Message::Ping(data) => ws.send(Message::Pong(data)).await.unwrap(),
                    _ => {}
                }
            }
        });

        let url = format!("ws://{addr}/codex/responses");
        let frame = response_create_frame(serde_json::json!({"model": "m", "input": []}));

        // Turn 1 completes (pooling its connection), then the backend closes it.
        let mut turn1 = open_simple(&url, HeaderMap::new(), &frame, Some("stale-1"))
            .await
            .expect("first turn connects");
        drain(&mut turn1).await;

        // The idle reader observes the server's Close and evicts the entry. (The
        // Close may land before or after the pool insert, so we assert the end
        // state — eviction — rather than the transient pooled state.)
        let evicted = tokio::time::timeout(Duration::from_secs(5), async {
            while pool_contains_for_tests("stale-1") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            evicted.is_ok(),
            "idle reader evicts a remotely-closed pooled connection"
        );

        // Turn 2 must open a fresh connection and stream without a mid-stream error.
        let mut turn2 = open_simple(&url, HeaderMap::new(), &frame, Some("stale-1"))
            .await
            .expect("second turn opens a fresh connection");
        let mut saw_error = false;
        while let Some(item) = turn2.recv().await {
            if item.is_err() {
                saw_error = true;
            }
        }
        assert!(
            !saw_error,
            "the replaced connection streamed turn 2 cleanly"
        );
        assert_eq!(
            accepts.load(Ordering::SeqCst),
            2,
            "turn 2 used a fresh connection, not the stale pooled one"
        );

        clear_pool_for_tests();
        server.abort();
    }

    /// Issue #93: downstream backpressure (a client not consuming events) must not
    /// starve WebSocket control-frame handling. The reader forwards events over an
    /// unbounded channel, so a mid-stream `Ping` is answered even while the client
    /// holds the receiver without reading.
    #[tokio::test]
    async fn backpressure_does_not_starve_control_frames() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let Some(Ok(Message::Text(_))) = ws.next().await else {
                panic!("expected a client frame");
            };
            // Emit a burst of data events the client will not consume yet.
            for _ in 0..8 {
                ws.send(Message::Text(
                    r#"{"type":"response.output_text.delta","delta":"x"}"#.to_string(),
                ))
                .await
                .unwrap();
            }
            // A control Ping in the middle of the unconsumed stream must still be
            // answered by the reader.
            ws.send(Message::Ping(b"mid".to_vec())).await.unwrap();
            let mut saw_pong = false;
            while let Some(message) = ws.next().await {
                if let Ok(Message::Pong(data)) = message {
                    assert_eq!(data, b"mid");
                    saw_pong = true;
                    break;
                }
            }
            assert!(
                saw_pong,
                "reader answered the Ping despite downstream backpressure"
            );
            ws.send(Message::Text(
                r#"{"type":"response.completed","response":{"id":"resp_1"}}"#.to_string(),
            ))
            .await
            .unwrap();
        });

        let url = format!("ws://{addr}/codex/responses");
        let frame = response_create_frame(serde_json::json!({"model": "m", "input": []}));
        // Acquire the stream but deliberately delay consuming it so events queue.
        let mut events = open_simple(&url, HeaderMap::new(), &frame, None)
            .await
            .expect("websocket should connect");

        // The reader must answer the Ping while we are NOT consuming events.
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server should observe a Pong before we consume events")
            .unwrap();

        // Now drain: every queued event (deltas + completed) is still delivered.
        let count = drain(&mut events).await;
        assert!(
            count >= 9,
            "queued events delivered after backpressure: {count}"
        );
    }

    /// A non-pooled turn (no session key) is used for exactly one turn, so once it
    /// completes the connection-owned reader must exit and close the socket rather
    /// than idling forever answering pings. The mock observes the client-side close
    /// the reader performs on exit; a leaked reader would keep the socket open and
    /// the read loop would hang until the timeout.
    #[tokio::test]
    async fn non_pooled_completed_turn_releases_socket() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let Some(Ok(Message::Text(_))) = ws.next().await else {
                panic!("expected a client frame");
            };
            for event in [
                r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
                r#"{"type":"response.completed","response":{"id":"resp_1"}}"#,
            ] {
                ws.send(Message::Text(event.to_string())).await.unwrap();
            }
            // Read to the close/EOF the reader performs after the single turn ends.
            loop {
                match ws.next().await {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => continue,
                }
            }
        });

        let url = format!("ws://{addr}/codex/responses");
        let frame = response_create_frame(serde_json::json!({"model": "m", "input": []}));
        let mut events = open_simple(&url, HeaderMap::new(), &frame, None)
            .await
            .expect("websocket should connect");
        drain(&mut events).await;

        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("reader closes the non-pooled socket after the turn completes")
            .unwrap();
    }

    /// A fresh `Turn` dropped without ever calling `stream()` must not leak its
    /// reader task and socket: `Turn`'s `Drop` signals the reader to shut down. The
    /// mock observes the client-side close the reader performs on exit.
    #[tokio::test]
    async fn abandoned_fresh_turn_releases_socket() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            // The client never sends a response.create frame; dropping the Turn must
            // still close the socket. A leaked reader would keep it open and hang.
            loop {
                match ws.next().await {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => continue,
                }
            }
        });

        let url = format!("ws://{addr}/codex/responses");
        let turn = begin(&url, HeaderMap::new(), None)
            .await
            .expect("handshake connects");
        drop(turn); // abandon the turn without streaming

        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("dropping a fresh turn closes its socket")
            .unwrap();
    }

    /// A turn command buffered as the reader exits (a `stream` that raced the
    /// reader's teardown) must surface a transport error on its event stream, not be
    /// dropped silently — which the caller would see as an empty "ghost" response.
    /// The channel is also closed, so a later racing dispatch fails fast.
    #[tokio::test]
    async fn buffered_command_on_reader_exit_surfaces_error() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<StartTurn>(1);
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let lock = Arc::new(AsyncMutex::new(()));
        cmd_tx
            .send(StartTurn {
                frame: Message::Text("{}".to_string()),
                events: ev_tx,
                record: RecordPlan::none(),
                slot: lock.clone().lock_owned().await,
            })
            .await
            .expect("buffer a command into the capacity-1 channel");

        fail_pending_commands(&mut cmd_rx);

        // The buffered command's stream gets an explicit error, not a bare close.
        match ev_rx.try_recv() {
            Ok(Err(error)) => assert!(
                error.message.contains("before the turn could start"),
                "unexpected error: {}",
                error.message
            ),
            other => panic!("expected a transport error, got {other:?}"),
        }
        // The drained command released its slot, so re-locking here cannot deadlock.
        // The channel is now closed, so a racing dispatch fails fast instead of
        // buffering into a reader that will never receive it.
        let (ev_tx2, _ev_rx2) = mpsc::unbounded_channel();
        assert!(
            cmd_tx
                .send(StartTurn {
                    frame: Message::Text("{}".to_string()),
                    events: ev_tx2,
                    record: RecordPlan::none(),
                    slot: lock.lock_owned().await,
                })
                .await
                .is_err(),
            "channel closed on reader exit rejects a late dispatch"
        );
    }

    /// `capture_continuation` pulls the response id (from `/response/id` or a
    /// top-level `id`), appends `output_item.done` items, and records the turn-state
    /// token from either the top level or `/response/turn_state`.
    #[test]
    fn capture_continuation_collects_id_items_and_turn_state() {
        let mut id = None;
        let mut items = Vec::new();
        let mut turn_state = None;

        capture_continuation(
            &parse_event(r#"{"type":"response.created","response":{"id":"resp_9"}}"#).unwrap(),
            &mut id,
            &mut items,
            &mut turn_state,
        );
        assert_eq!(id.as_deref(), Some("resp_9"));

        // A response-level event with only a top-level `id` uses the fallback.
        let mut id_top = None;
        capture_continuation(
            &parse_event(r#"{"type":"response.done","id":"resp_top"}"#).unwrap(),
            &mut id_top,
            &mut items,
            &mut turn_state,
        );
        assert_eq!(id_top.as_deref(), Some("resp_top"));

        capture_continuation(
            &parse_event(
                r#"{"type":"response.output_item.done","item":{"type":"message","id":"m1"}}"#,
            )
            .unwrap(),
            &mut id,
            &mut items,
            &mut turn_state,
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], "m1");

        // turn_state at the top level.
        capture_continuation(
            &parse_event(r#"{"type":"response.output_text.delta","turn_state":"ts-1"}"#).unwrap(),
            &mut id,
            &mut items,
            &mut turn_state,
        );
        assert_eq!(turn_state.as_deref(), Some("ts-1"));

        // turn_state nested under /response.
        let mut nested_state = None;
        capture_continuation(
            &parse_event(r#"{"type":"response.completed","response":{"turn_state":"ts-2"}}"#)
                .unwrap(),
            &mut id,
            &mut items,
            &mut nested_state,
        );
        assert_eq!(nested_state.as_deref(), Some("ts-2"));
    }

    /// A rejected `previous_response_id` is detected from either the error `code` or
    /// a case-insensitive "previous response ... not found" `message`.
    #[test]
    fn detects_previous_response_missing_by_code_and_message() {
        assert!(is_previous_response_missing(&serde_json::json!({
            "error": {"code": "previous_response_not_found"}
        })));
        assert!(is_previous_response_missing(&serde_json::json!({
            "error": {"message": "The Previous response was Not Found"}
        })));
        assert!(!is_previous_response_missing(&serde_json::json!({
            "error": {"code": "rate_limited", "message": "slow down"}
        })));
        assert!(!is_previous_response_missing(&serde_json::json!({
            "type": "response.completed"
        })));
    }

    /// A non-HTTP handshake failure has no status and is wrapped as a transport error.
    #[test]
    fn map_handshake_error_wraps_non_http_error() {
        let error = map_handshake_error(tungstenite::Error::ConnectionClosed);
        assert!(error.status.is_none());
        assert!(
            error.message.contains("connect error"),
            "unexpected message: {}",
            error.message
        );
    }

    /// An unexpected binary frame mid-turn is surfaced as a transport error, not
    /// silently skipped.
    #[tokio::test]
    async fn binary_frame_during_turn_surfaces_error() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let Some(Ok(Message::Text(_))) = ws.next().await else {
                panic!("expected a client frame");
            };
            ws.send(Message::Binary(vec![0, 1, 2])).await.unwrap();
            let _ = ws.next().await; // observe the reader's close on exit
        });

        let url = format!("ws://{addr}/codex/responses");
        let frame = response_create_frame(serde_json::json!({"model": "m", "input": []}));
        let mut events = open_simple(&url, HeaderMap::new(), &frame, None)
            .await
            .expect("websocket should connect");

        let mut saw_binary_error = false;
        while let Some(item) = events.recv().await {
            if let Err(error) = item {
                assert!(
                    error.message.contains("binary"),
                    "unexpected error: {}",
                    error.message
                );
                saw_binary_error = true;
            }
        }
        assert!(
            saw_binary_error,
            "a binary frame surfaces a transport error"
        );
        server.abort();
    }

    /// An abrupt stream end (socket dropped) before a terminal event surfaces a
    /// transport error rather than a silently short, fake-success response.
    #[tokio::test]
    async fn stream_dropped_before_terminal_surfaces_error() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let Some(Ok(Message::Text(_))) = ws.next().await else {
                panic!("expected a client frame");
            };
            // Drop the socket without a terminal event or a Close frame.
            drop(ws);
        });

        let url = format!("ws://{addr}/codex/responses");
        let frame = response_create_frame(serde_json::json!({"model": "m", "input": []}));
        let mut events = open_simple(&url, HeaderMap::new(), &frame, None)
            .await
            .expect("websocket should connect");

        let mut saw_error = false;
        while let Some(item) = events.recv().await {
            if item.is_err() {
                saw_error = true;
            }
        }
        assert!(
            saw_error,
            "an abrupt stream end before a terminal event surfaces an error"
        );
        server.await.unwrap();
    }
}
