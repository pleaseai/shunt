//! The Codex Responses WebSocket v2 transport (issue #32): reuse the session's
//! pooled connection, send only the continuation delta when the input is an
//! append-only extension, and peek the first event so a pre-first-token failure
//! can transparently fall back to HTTP.

use std::borrow::Cow;

use axum::{
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
};
use serde_json::{json, Value};

use crate::{
    adapters::AdapterError, auth::Credential, config::AuthMode, error::ShuntError,
    model::responses::ResponseEvent, routing::Route, server::AppState,
};

use super::codex_continuation;
use super::codex_ws::{self, CodexWsError, CodexWsEvents};
use super::context::ForwardOptions;
use super::early_stream::bounded_input_estimate;
use super::error::build_upstream_error;
use super::request::{responses_url, routing_hint, CODEX_CLIENT_VERSION, CODEX_USER_AGENT};
use super::ws_stream::{json_events_response, stream_events_response};

/// The identity inputs for a turn's handshake headers: the connection pool
/// key (the turn's thread scope — the child's on a delegated turn — and
/// account-prefixed on the pool path), the window-counter key (the same scope
/// unprefixed by the account, so an account rotation cannot reset the
/// conversation's window), the effective session id, the request's one-shot
/// compaction mark (consumed by this turn's window computation if no earlier
/// dispatch of the same turn reached an upstream), and the delegated-turn
/// subagent identity (child thread id + markers) when the turn is a child's.
pub(super) struct WsIdentity<'a> {
    pub(super) pool_key: Option<&'a str>,
    pub(super) window_key: Option<&'a str>,
    pub(super) session_id: Option<&'a str>,
    pub(super) compact: crate::request::CompactionMark,
    pub(super) delegation: Option<&'a super::request::CodexDelegation>,
}

/// Drive a turn over the Codex Responses WebSocket v2 transport (issue #32).
/// Reuses the session's pooled connection and, when the current input is an
/// append-only extension of the previous turn, sends only the delta with
/// `previous_response_id` (the payload-reduction lever). Events are re-encoded
/// through the same [`AnthropicSseMachine`] the HTTP path uses; a rejected
/// handshake is re-shaped exactly like an HTTP upstream error.
pub(super) async fn forward_websocket(
    state: &AppState,
    route: &Route,
    identity: WsIdentity<'_>,
    forward: ForwardOptions,
    credential: Credential,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let ForwardOptions {
        upstream_body,
        auth,
        turn,
        codex_quota_account,
        estimate_input,
        started_at: _,
        // The websocket attempt's mark and window key arrive through
        // `identity` below; `forward`'s copies exist for the HTTP fallback,
        // which this function never drives itself.
        window_key: _,
        compact: _,
    } = forward;
    let WsIdentity {
        pool_key,
        window_key,
        session_id,
        compact,
        delegation,
    } = identity;
    let pool_key = pool_key.filter(|key| !key.is_empty());
    let http_url = responses_url(&state.config, &route.provider);
    let ws_url = codex_ws::to_websocket_url(&http_url).map_err(ws_transport_error)?;
    // The handshake's `x-codex-window-id` carries the conversation's compaction
    // window: the request's one-shot mark is consumed here — the first window
    // computation of the turn, which is the first dispatch that reaches an
    // upstream — bumping the counter and rotating the socket; every later
    // dispatch of the same turn (route failover, a gated REDO, the HTTP
    // fallback below) takes an already-consumed mark and reads the advanced
    // window. The counter keys on `window_key` — the turn's thread scope (the
    // child's on a delegated turn), unprefixed by the account name so an
    // account rotation cannot reset it — and the bump sweeps the
    // conversation's sockets under every account, not just this attempt's
    // (see `window_for_turn`).
    let window = codex_ws::window_for_turn(window_key, compact.take());
    let ctx = WsTurnContext {
        ws_url,
        pool_key,
        session_id: session_id.filter(|id| !id.is_empty()),
        provider: &route.provider,
        accounts: std::sync::Arc::clone(&state.accounts),
        codex_quota_account: codex_quota_account.as_ref(),
        credential,
        auth,
        signature: codex_continuation::signature(&upstream_body),
        upstream_body,
        routing_hint: routing_hint(route),
        window,
        delegation,
    };
    // The internal key composes with a control-byte separator; render it with
    // `:` so the log line stays readable.
    tracing::debug!(
        provider = %route.provider,
        ws_url = %ctx.ws_url,
        pool_key = pool_key
            .unwrap_or("")
            .replace(codex_ws::KEY_COMPONENT_SEPARATOR, ":"),
        "opening codex websocket"
    );

    // Overlap the CPU-bound tiktoken encode with the websocket connect (same
    // rationale as forward_http); its result is only consumed once the event
    // stream begins. If open_ws_turn fails, this handle is dropped un-awaited and
    // forward_http re-spawns its own encode on the fallback path — a rare,
    // off-executor double-encode we accept rather than thread the handle back out
    // through the Err path (which would couple the transport signatures).
    let estimate_handle = estimate_input.map(|request| {
        tokio::task::spawn_blocking(move || crate::count_tokens::count_input_tokens_value(&request))
    });
    let (buffered, first_at, events) = open_ws_turn(&ctx, turn.response_bounds.idle).await?;
    // Both branches consume it: the streaming arm seeds `message_start`, and a
    // non-streaming turn cut short by an emulated stop sequence needs it because
    // the stop makes the upstream's own usage a no-op (issue #605).
    //
    // Bounded, unlike the bare `handle.await` this replaces: the turn is already
    // open by now, so blocking here stops the collector consuming events and
    // backpressures the bounded `CodexWsEvents` channel until tokenization ends.
    // The encode has had the whole `open_ws_turn` to finish, so the bound only
    // bites when the blocking pool is saturated — the same trade the HTTP and
    // pooled paths already make.
    let input_tokens_estimate = match estimate_handle {
        Some(handle) => bounded_input_estimate(handle, std::time::Duration::from_secs(1)).await,
        None => 0,
    };
    if turn.client_wants_stream {
        let keepalive = std::time::Duration::from_secs(state.config.server.sse_keepalive_seconds);
        Ok((
            StatusCode::OK,
            stream_events_response(
                buffered,
                events,
                turn.relay(route),
                input_tokens_estimate,
                keepalive,
            ),
        ))
    } else {
        // See `forward_http`: surface the real status (a `502` when a backend
        // error event fired, issue #113) to the access log and metrics rather
        // than a hardcoded `200`.
        let response = json_events_response(
            buffered,
            events,
            turn.relay(route),
            input_tokens_estimate,
            turn.response_bounds,
            first_at,
        )
        .await?;
        Ok((response.status(), response))
    }
}

/// Everything needed to (re)start a websocket turn for a request.
struct WsTurnContext<'a> {
    ws_url: String,
    pool_key: Option<&'a str>,
    /// The effective conversation id (the inbound `x-claude-code-session-id`
    /// header or a parsed metadata session), which becomes the handshake
    /// session-identity headers (the backend derives prompt-cache affinity
    /// from `session-id`) and the body's `prompt_cache_key`.
    session_id: Option<&'a str>,
    provider: &'a str,
    /// Shared, not borrowed: the `codex.rate_limits` tap outlives this context
    /// (the connection reader owns it for the turn's duration).
    accounts: std::sync::Arc<crate::accounts::AccountPool>,
    codex_quota_account: Option<&'a crate::config::AccountConfig>,
    credential: Credential,
    auth: AuthMode,
    signature: String,
    upstream_body: std::sync::Arc<Value>,
    /// The `x-codex-routing-hint` value for this turn, built once from the route,
    /// or `None` when the client-controlled model makes an unusable value (see
    /// [`routing_hint`]). Applied as a handshake header, so on a *reused* pooled
    /// connection the hint the backend saw is the one from the turn that opened
    /// the socket, not this turn's. That mirrors upstream, which likewise only
    /// builds it at connection time — and it is a routing *hint*, not a routing
    /// decision, so a stale one costs nothing.
    routing_hint: Option<HeaderValue>,
    /// The compaction-window index the handshake's `x-codex-window-id` carries
    /// (`{session}:{window}`). Computed once per turn in [`forward_websocket`]:
    /// a compaction-marked turn bumps the pooled counter, every other turn
    /// reads the current window, and an unpoolable session stays 0.
    window: u64,
    /// The delegated-turn subagent identity for the handshake headers, when
    /// this turn is a child's (`{session}::{agent}` thread id + the two
    /// markers); `None` sends the plain session headers.
    delegation: Option<&'a super::request::CodexDelegation>,
}

/// The first event, peeked off the stream before the websocket response is
/// committed, then replayed ahead of the rest of the channel so the peek costs no
/// event. See [`open_ws_turn`] for why every turn is peeked.
pub(super) type BufferedEvent = Option<Result<ResponseEvent, CodexWsError>>;

/// Open a websocket turn, applying `previous_response_id` continuation when the
/// connection is reused and the input is an append-only extension. If the backend
/// rejects the replayed id, retry once with the full input on a fresh connection.
///
/// The first event is always peeked before the response is committed, extending
/// the pre-handshake HTTP safety net across the send→first-event window (issue
/// #46). `Turn::stream` only *queues* the frame; the connection reader sends it
/// and produces the first event asynchronously, so a socket that dies between the
/// send and the first event — an idle-eviction race, a backend hiccup, a network
/// blip — would otherwise surface as an error event on an already-committed
/// stream. Peeking lets [`commit_or_fallback`] catch that failure while nothing
/// has reached the client yet and return `Err`, so [`forward`] transparently
/// re-drives the whole turn over HTTP. If instead the backend accepts the frame
/// but never produces a first event, the peek waits up to the reader's idle
/// timeout before falling back — a bounded cost that still never does worse than
/// plain HTTP against the same unresponsive backend. Only once the first event is
/// in hand is the turn under way; a failure after that is genuinely mid-stream and
/// surfaces as a clean error: an Anthropic `error` event for a streaming client
/// ([`stream_events_response`]), a gateway error for a non-streaming one
/// ([`json_events_response`]).
///
/// `idle` is the gated call's `gated_idle_ms` and `None` for every other turn.
/// With it set, the clock starts as the turn is opened, as `forward_http`'s
/// starts at its send: the handshake (or a pooled connection's probe), the
/// frame, and the wait for the first event are one gap (#690), and a stall
/// anywhere in it is a cut, not an HTTP fallback (see [`peek_first_event`]).
/// The retry below is a new request and starts its own clock. The first
/// event's arrival instant is returned for the collector to measure its next
/// gap from.
async fn open_ws_turn(
    ctx: &WsTurnContext<'_>,
    idle: Option<std::time::Duration>,
) -> Result<(BufferedEvent, Option<tokio::time::Instant>, CodexWsEvents), AdapterError> {
    let (first, first_at, events, used_continuation) = open_and_peek(ctx, true, idle).await?;
    // A rejected previous_response_id arrives before any output: retry once with
    // the full input on a fresh connection, then evaluate that stream instead.
    if used_continuation && matches!(&first, Some(Err(error)) if error.previous_response_missing) {
        tracing::info!("codex previous_response_id rejected; retrying with full input");
        let (first, first_at, events, _) = open_and_peek(ctx, false, idle).await?;
        let (first, events) = commit_or_fallback(first, events, ctx.auth)?;
        return Ok((first, first_at, events));
    }
    let (first, events) = commit_or_fallback(first, events, ctx.auth)?;
    Ok((first, first_at, events))
}

/// [`start_ws_turn`] then [`peek_first_event`], both under one gated idle
/// clock started here. `None` reads no clock.
async fn open_and_peek(
    ctx: &WsTurnContext<'_>,
    allow_continuation: bool,
    idle: Option<std::time::Duration>,
) -> Result<
    (
        BufferedEvent,
        Option<tokio::time::Instant>,
        CodexWsEvents,
        bool,
    ),
    AdapterError,
> {
    let clock = idle.map(|idle| (idle, tokio::time::Instant::now()));
    let (events, used_continuation) =
        crate::adapters::within_idle(clock, start_ws_turn(ctx, allow_continuation))
            .await
            .map_err(crate::adapters::idle_error)??;
    let (first, first_at, events) = peek_first_event(events, clock).await?;
    Ok((first, first_at, events, used_continuation))
}

/// Await the first event of a freshly opened turn, returning it alongside the
/// still-live channel so it can be replayed before the remainder of the stream.
///
/// With `clock` set — the gated call's gap and the instant [`open_and_peek`]
/// opened the turn — the wait ends at the close of that first gap, which the
/// handshake and the frame already spent part of: the twin of the gap
/// `forward_http` times from its send, the header wait included (#690). A
/// backend that accepts the frame and then says nothing is cut there with the
/// idle marker rather than held to the transport's own idle timeout. That is a
/// cut, not an HTTP fallback: the bound belongs to the call, and re-driving
/// the turn over HTTP would start it again against a gap already spent.
/// Dropping `events` abandons the turn, so its socket is evicted rather than
/// pooled.
///
/// With `clock` set, the first event's arrival is returned too: it is the
/// upstream's last progress, and the collector's next gap runs from it rather
/// than from whenever the collector starts after the bounded estimate wait
/// (#690). `None` for every other turn, which reads no clock.
async fn peek_first_event(
    mut events: CodexWsEvents,
    clock: Option<(std::time::Duration, tokio::time::Instant)>,
) -> Result<(BufferedEvent, Option<tokio::time::Instant>, CodexWsEvents), AdapterError> {
    let first = crate::adapters::within_idle(clock, events.recv())
        .await
        .map_err(crate::adapters::idle_error)?;
    let first_at = clock.map(|_| tokio::time::Instant::now());
    Ok((first, first_at, events))
}

/// Decide, from the peeked first event, whether to commit to the websocket
/// response or fall back to HTTP. A delivered first event (`Ok`) means the turn is
/// under way: buffer it for replay and stream the socket. A transport error or an
/// empty stream means nothing ever reached the client, so return `Err` to let
/// [`forward`] re-drive the turn over HTTP transparently — the send→first-event
/// analogue of the pre-handshake fallback. Backend-sent error *events* (a rate
/// limit, a content-policy refusal) arrive as `Ok` and are streamed through rather
/// than retried — except a first event that is a wrapped HTTP-class error
/// ([`wrapped_first_event_error`]), which is re-shaped like a refused handshake.
fn commit_or_fallback(
    first: BufferedEvent,
    events: CodexWsEvents,
    auth: AuthMode,
) -> Result<(BufferedEvent, CodexWsEvents), AdapterError> {
    match first {
        Some(Ok(event)) => match wrapped_first_event_error(&event, auth) {
            Some(error) => Err(error),
            None => Ok((Some(Ok(event)), events)),
        },
        Some(Err(error)) => Err(ws_transport_error(error)),
        None => Err(ws_before_headers_error(
            "codex websocket closed before any event".to_string(),
        )),
    }
}

/// Classify a peeked first event that is the Codex websocket's wrapped error
/// frame, mirroring openai/codex rust-v0.156.0's
/// `map_wrapped_websocket_error_event`. The backend delivers HTTP-class errors
/// as `{"type":"error","status":400,"error":{..},"headers":{..}}` on the socket,
/// and as the first event nothing has reached the client yet:
///
/// - `websocket_connection_limit_reached` is retryable upstream, so it becomes
///   the same pre-header transport failure a dropped socket is.
/// - A non-2xx `status` / `status_code` is handled exactly like a refused
///   handshake ([`ws_connect_error`]): [`build_upstream_error`] with that status,
///   the frame's `retry-after` header, and the frame itself as the body.
///
/// Any other event (including an error frame without a status) returns `None`
/// and commits. The socket is never pooled either way: `error` is a terminal
/// event that the connection reader evicts on.
fn wrapped_first_event_error(event: &ResponseEvent, auth: AuthMode) -> Option<AdapterError> {
    if event.event.as_deref() != Some("error") {
        return None;
    }
    let data = &event.data;
    if data.pointer("/error/code").and_then(Value::as_str)
        == Some("websocket_connection_limit_reached")
    {
        return Some(ws_before_headers_error(
            "codex websocket connection limit reached".to_string(),
        ));
    }
    let status = crate::model::responses::wrapped_error_status(data)?;
    let retry_after = data
        .get("headers")
        .and_then(Value::as_object)
        .and_then(|headers| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        })
        .and_then(|(_, value)| match value {
            Value::String(value) => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        });
    Some(build_upstream_error(
        status,
        retry_after,
        data.to_string(),
        auth,
    ))
}

/// Rewrite `frame_body` in place for a continuation turn: replace `input` with the
/// decision's delta, set `previous_response_id`, and — when a turn-state token is
/// present — echo it into `client_metadata` as `x-codex-turn-state`. Returns the
/// delta item count (for logging). Pure and unit-tested; the async turn setup that
/// produces the inputs stays in [`start_ws_turn`].
fn apply_continuation(
    frame_body: &mut Value,
    decision: &codex_continuation::Decision,
    turn_state: Option<&str>,
) -> usize {
    if let Some(object) = frame_body.as_object_mut() {
        object.insert("input".to_string(), json!(decision.input_delta));
        object.insert(
            "previous_response_id".to_string(),
            json!(decision.previous_response_id),
        );
        if let Some(turn_state) = turn_state {
            let metadata = object
                .entry("client_metadata".to_string())
                .or_insert_with(|| json!({}));
            // Defensive: a pre-existing non-object `client_metadata` would make
            // `as_object_mut()` return None and silently drop the turn-state token;
            // reset it to an object so the token is always recorded.
            if !metadata.is_object() {
                *metadata = json!({});
            }
            if let Some(metadata) = metadata.as_object_mut() {
                metadata.insert("x-codex-turn-state".to_string(), json!(turn_state));
            }
        }
    }
    decision.input_delta.len()
}

/// Begin a turn on the session's connection and send its frame. When
/// `allow_continuation` and the reused connection's stored state make the input an
/// append-only extension, send only the delta with `previous_response_id`;
/// otherwise send the full input. Returns the event stream and whether the delta
/// path was taken.
async fn start_ws_turn(
    ctx: &WsTurnContext<'_>,
    allow_continuation: bool,
) -> Result<(CodexWsEvents, bool), AdapterError> {
    let headers = websocket_headers(
        ctx.credential.clone(),
        ctx.routing_hint.as_ref(),
        ctx.session_id,
        ctx.window,
        ctx.delegation,
    )?;
    let turn = codex_ws::begin(&ctx.ws_url, headers, ctx.pool_key, ctx.provider)
        .await
        .map_err(|error| ws_connect_error(error, ctx.auth))?;
    // Only a fresh connection performed a handshake this turn, so only it carries
    // quota headers to record. A reused connection has no new *header* signal to
    // report — replaying its original handshake headers would overwrite fresher
    // state with stale values and falsely bump the observation timestamp (issue:
    // stale quota marks outliving their upstream reset). The in-stream
    // `codex.rate_limits` event tapped below covers every turn, reused included.
    if let (Some(account), Some(headers)) = (ctx.codex_quota_account, turn.handshake_headers()) {
        ctx.accounts
            .note_codex_quota(ctx.provider, account, headers);
    }

    // Non-continuation turns never edit the translated body, so they avoid paying
    // for a full copy.
    let mut frame_body = Cow::Borrowed(ctx.upstream_body.as_ref());
    let mut used_continuation = false;
    if allow_continuation {
        // Only a reused connection carries stored continuation state; a fresh
        // connection has no chance to continue and is not counted, so the hit/
        // fallback series stay directly comparable (issue #45).
        if let Some(stored) = turn.stored_continuation() {
            match codex_continuation::decide_with_signature(
                &stored,
                ctx.upstream_body.as_ref(),
                &ctx.signature,
            ) {
                Some(decision) => {
                    let turn_state = stored
                        .turn_state
                        .as_deref()
                        .or_else(|| turn.handshake_turn_state());
                    let delta_items =
                        apply_continuation(frame_body.to_mut(), &decision, turn_state);
                    used_continuation = true;
                    tracing::debug!(
                        delta_items,
                        "codex websocket continuing with previous_response_id"
                    );
                    crate::metrics::record_continuation_outcome(
                        ctx.provider,
                        crate::metrics::ContinuationOutcome::Hit,
                    );
                }
                None => {
                    // The reused connection's stored transcript was not an
                    // append-only prefix of this input (history rewrite, a changed
                    // non-input field, or normalization drift), so re-send the full
                    // input. Correct, but the payload-trim was missed.
                    tracing::debug!(
                        "codex websocket reused connection but input diverged; re-sending full input"
                    );
                    crate::metrics::record_continuation_outcome(
                        ctx.provider,
                        crate::metrics::ContinuationOutcome::Fallback,
                    );
                }
            }
        }
    }

    let frame = codex_ws::response_create_frame(frame_body.as_ref());
    // The websocket backend reports its rate limits in-stream rather than only on
    // the handshake, so tap the event to keep a pooled Codex account's quota
    // observed on every turn — including turns that reused a connection.
    let rate_limits = ctx.codex_quota_account.map(|account| {
        let accounts = std::sync::Arc::clone(&ctx.accounts);
        let provider = ctx.provider.to_string();
        let account = account.clone();
        std::sync::Arc::new(move |event: &Value| {
            accounts.note_codex_rate_limits(&provider, &account, event);
        }) as codex_ws::RateLimitTap
    });
    let record = codex_ws::RecordPlan {
        signature: ctx.signature.clone(),
        request: Some(std::sync::Arc::clone(&ctx.upstream_body)),
        rate_limits,
    };
    let events = turn
        .stream(&frame, record)
        .await
        .map_err(|error| ws_connect_error(error, ctx.auth))?;
    Ok((events, used_continuation))
}

/// Codex identity + beta-protocol headers for the websocket upgrade. Mirrors the
/// ChatGPT/Codex arm of [`request_builder`] but swaps `OpenAI-Beta` for the
/// websocket protocol value. Only the ChatGPT OAuth credential reaches here
/// (the transport is gated to that backend); other credential shapes still send
/// their bearer so a misconfiguration fails upstream rather than silently
/// unauthenticated.
fn websocket_headers(
    credential: Credential,
    routing_hint: Option<&HeaderValue>,
    session_id: Option<&str>,
    window: u64,
    delegation: Option<&super::request::CodexDelegation>,
) -> Result<HeaderMap, AdapterError> {
    let mut headers = HeaderMap::new();
    // Set only on the ChatGPT OAuth arm; inserted after the match (see there).
    let mut hint = None;
    let mut set = |name: &'static str, value: String| -> Result<(), AdapterError> {
        let value = HeaderValue::from_str(&value).map_err(|error| {
            let message = format!("invalid {name} header: {error}");
            let response = ShuntError::bad_gateway(message.clone()).into_response();
            AdapterError {
                message,
                response: Box::new(response),
                failure: Some(crate::adapters::AdapterFailure::BeforeHeaders),
            }
        })?;
        headers.insert(name, value);
        Ok(())
    };
    set("openai-beta", codex_ws::WEBSOCKET_BETA_PROTOCOL.to_string())?;
    match credential {
        Credential::ChatGptOAuth {
            access_token,
            account_id,
        } => {
            set("authorization", format!("Bearer {access_token}"))?;
            set("chatgpt-account-id", account_id)?;
            set("originator", "codex_cli_rs".to_string())?;
            set("user-agent", CODEX_USER_AGENT.to_string())?;
            set("version", CODEX_CLIENT_VERSION.to_string())?;
            // Same session identity the HTTP transport sends: the backend
            // derives prompt-cache affinity from `session-id`, and its value
            // must equal the body's `prompt_cache_key` (see `request.rs`).
            // The window id advances on compaction-marked turns (see
            // `forward_websocket`); the HTTP transport stays `:0`.
            // A delegated turn swaps `thread-id` for the child's derived id
            // and adds the two subagent markers, and every thread-derived id
            // — `x-client-request-id` and the window id's identity part —
            // carries the child's, exactly as codex builds them from its
            // thread metadata.
            if let Some(session_id) = session_id.filter(|id| !id.is_empty()) {
                let thread_id =
                    delegation.map_or(session_id, |delegation| delegation.thread_id.as_str());
                set("session-id", session_id.to_string())?;
                match delegation {
                    Some(delegation) => {
                        set("thread-id", delegation.thread_id.clone())?;
                        set(
                            "x-codex-parent-thread-id",
                            delegation.parent_thread_id.clone(),
                        )?;
                        set("x-openai-subagent", delegation.subagent.clone())?;
                    }
                    None => set("thread-id", session_id.to_string())?,
                }
                set("x-client-request-id", thread_id.to_string())?;
                set("x-codex-window-id", format!("{thread_id}:{window}"))?;
            }
            // Deliberately not through `set`: every other header must fail the
            // turn on a malformed value, but the routing hint is built from the
            // client-controlled model and was already validated (or omitted) by
            // `routing_hint`, so it can only be inserted or skipped. Deferred
            // past the match because `set` holds the mutable borrow of `headers`.
            hint = routing_hint.cloned();
        }
        Credential::ApiKey { value, .. } => set("authorization", format!("Bearer {value}"))?,
        Credential::XaiOauth { access_token } => {
            set("authorization", format!("Bearer {access_token}"))?
        }
        Credential::CursorOauth { access_token } => {
            set("authorization", format!("Bearer {access_token}"))?
        }
        Credential::ClaudeOauth { access_token, .. } => {
            set("authorization", format!("Bearer {access_token}"))?
        }
        Credential::GoogleOauth { access_token, .. } => {
            set("authorization", format!("Bearer {access_token}"))?
        }
        // Fails closed for the same reason as the HTTP Responses path: the
        // Codex WebSocket is not the origin an Antigravity subscription token
        // was issued for, so an unreachable-by-validation arm must not carry it.
        Credential::AntigravityOauth { .. } => {}
        Credential::Passthrough => {}
        // Kimi's coding API speaks the Anthropic Messages shape, so a
        // `kimi_oauth` provider is always `kind = "anthropic"` and never
        // reaches this Responses/Codex WebSocket path in practice.
        Credential::KimiOauth { .. } => {}
    }
    if let Some(hint) = hint {
        headers.insert("x-codex-routing-hint", hint);
    }
    Ok(headers)
}

fn ws_transport_error(error: CodexWsError) -> AdapterError {
    ws_before_headers_error(error.message)
}

/// Every websocket failure before connection or the first event is safe to
/// replay over HTTP. Failures after the first event remain unclassified so the
/// fallback gates stop instead of duplicating an accepted turn.
fn ws_before_headers_error(message: String) -> AdapterError {
    let response = ShuntError::bad_gateway(message.clone()).into_response();
    AdapterError {
        message,
        response: Box::new(response),
        failure: Some(crate::adapters::AdapterFailure::BeforeHeaders),
    }
}

/// Map a websocket handshake failure to an [`AdapterError`]. A refused upgrade
/// carries an HTTP status/body, so it re-shapes through the shared
/// [`build_upstream_error`]; a pure transport failure (DNS, TLS, timeout) maps to
/// 502 like a failed HTTP send.
fn ws_connect_error(error: CodexWsError, auth: AuthMode) -> AdapterError {
    match error.status {
        Some(status) => build_upstream_error(status, error.retry_after, error.body, auth),
        None => {
            tracing::warn!(reason = %error.message, "codex websocket transport failure");
            ws_transport_error(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::{json, Value};

    use crate::adapters::responses::codex_continuation::Decision;

    use super::apply_continuation;

    /// A well-formed routing hint for the header tests that are not about the
    /// hint itself.
    fn hint() -> axum::http::HeaderValue {
        axum::http::HeaderValue::from_static("model=gpt-5.2-codex")
    }

    #[test]
    fn apply_continuation_sets_delta_previous_id_and_turn_state() {
        // A continuation turn replaces `input` with the delta, sets
        // `previous_response_id`, and echoes the turn-state token.
        let mut frame = json!({"model": "m", "input": ["FULL_INPUT"]});
        let decision = Decision {
            previous_response_id: "resp_1".to_string(),
            input_delta: vec![json!({"type": "message", "role": "user"})],
        };
        let delta_items = apply_continuation(&mut frame, &decision, Some("ts_1"));
        assert_eq!(delta_items, 1);
        assert_eq!(frame["input"], json!([{"type": "message", "role": "user"}]));
        assert_eq!(frame["previous_response_id"], json!("resp_1"));
        assert_eq!(
            frame["client_metadata"]["x-codex-turn-state"],
            json!("ts_1")
        );
    }

    #[test]
    fn apply_continuation_without_turn_state_omits_metadata() {
        // No turn-state token ⇒ no `client_metadata` is synthesized.
        let mut frame = json!({"input": ["FULL_INPUT"]});
        let decision = Decision {
            previous_response_id: "r".to_string(),
            input_delta: vec![json!("a"), json!("b")],
        };
        let delta_items = apply_continuation(&mut frame, &decision, None);
        assert_eq!(delta_items, 2);
        assert_eq!(frame["input"], json!(["a", "b"]));
        assert_eq!(frame["previous_response_id"], json!("r"));
        assert!(frame.get("client_metadata").is_none());
    }

    #[test]
    fn apply_continuation_overwrites_non_object_client_metadata() {
        // A pre-existing non-object `client_metadata` is reset to an object so the
        // turn-state token is recorded rather than silently dropped.
        let mut frame = json!({"input": [], "client_metadata": "not-an-object"});
        let decision = Decision {
            previous_response_id: "r".to_string(),
            input_delta: vec![json!("x")],
        };
        apply_continuation(&mut frame, &decision, Some("ts"));
        assert_eq!(frame["client_metadata"]["x-codex-turn-state"], json!("ts"));
    }

    #[test]
    fn apply_continuation_merges_into_existing_client_metadata() {
        // The turn-state token is inserted alongside pre-existing metadata keys,
        // not clobbering them (the `or_insert_with` existing-object path).
        let mut frame = json!({"input": [], "client_metadata": {"existing": "keep"}});
        let decision = Decision {
            previous_response_id: "r".to_string(),
            input_delta: vec![json!("x")],
        };
        apply_continuation(&mut frame, &decision, Some("ts"));
        assert_eq!(frame["client_metadata"]["existing"], json!("keep"));
        assert_eq!(frame["client_metadata"]["x-codex-turn-state"], json!("ts"));
    }

    /// `commit_or_fallback` classifies the peeked first event: a delivered event
    /// commits to the websocket (buffered for replay), while a transport error or
    /// an empty stream returns `Err` so [`super::forward`] re-drives over HTTP. The
    /// empty-stream arm is unreachable from the integration mocks (which always
    /// send an event or a transport error before closing), so it is exercised here.
    #[test]
    fn commit_or_fallback_classifies_the_peeked_first_event() {
        use super::{commit_or_fallback, AuthMode, CodexWsError, ResponseEvent};
        use tokio::sync::mpsc;

        // A delivered first event commits: it is buffered for replay and the
        // channel is handed back intact.
        let (_tx, rx) = mpsc::channel(16);
        let event = ResponseEvent {
            event: Some("response.created".to_string()),
            data: Value::Null,
        };
        let (buffered, _events) = commit_or_fallback(Some(Ok(event)), rx, AuthMode::ChatgptOauth)
            .expect("a delivered event commits");
        assert!(
            matches!(buffered, Some(Ok(_))),
            "the first event is buffered for replay"
        );

        // A transport error before the first event falls back to HTTP.
        let (_tx, rx) = mpsc::channel(16);
        let error = CodexWsError {
            status: None,
            retry_after: None,
            body: String::new(),
            message: "socket dropped before first event".to_string(),
            previous_response_missing: false,
        };
        assert!(
            commit_or_fallback(Some(Err(error)), rx, AuthMode::ChatgptOauth).is_err(),
            "a pre-first-event transport error falls back to HTTP"
        );

        // An empty stream (channel closed before any event) also falls back and
        // carries the pre-header classification consumed by both fallback gates.
        let (_tx, rx) = mpsc::channel(16);
        let error = commit_or_fallback(None, rx, AuthMode::ChatgptOauth)
            .expect_err("an empty stream falls back to HTTP");
        assert_eq!(
            error.failure,
            Some(crate::adapters::AdapterFailure::BeforeHeaders)
        );
    }

    /// `peek_first_event` hands back when the first event arrived — the
    /// instant `json_events_response` measures its next gap from (#690) — and
    /// reads no clock on a client turn.
    ///
    /// Non-vacuity: take the instant before the wait rather than after it and
    /// `first_at` is the peek's start, 120 ms early, so this goes red.
    #[tokio::test(start_paused = true)]
    async fn peek_first_event_returns_the_first_events_arrival_instant() {
        async fn peek_one(
            idle: Option<std::time::Duration>,
        ) -> (tokio::time::Instant, Option<tokio::time::Instant>) {
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            let started = tokio::time::Instant::now();
            let clock = idle.map(|idle| (idle, started));
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                let event = super::ResponseEvent {
                    event: Some("response.created".to_string()),
                    data: Value::Null,
                };
                let _ = tx.send(Ok(event)).await;
            });
            let (first, first_at, _events) = super::peek_first_event(rx, clock)
                .await
                .expect("an event inside the gap is peeked");
            assert!(matches!(first, Some(Ok(_))), "the first event is returned");
            (started, first_at)
        }

        let (started, first_at) = peek_one(Some(std::time::Duration::from_millis(300))).await;
        assert_eq!(
            first_at,
            Some(started + std::time::Duration::from_millis(120)),
            "a gated peek returns the first event's arrival"
        );

        let (_, first_at) = peek_one(None).await;
        assert_eq!(first_at, None, "a client turn reads no clock");
    }

    /// The peek's wait ends at the close of the gap the turn's opening started
    /// — the handshake and the frame spent part of it (#690) — not a full gap
    /// after the peek begins. With 200 ms spent opening and a 300 ms gap, a
    /// first event 150 ms into the peek is past the gap and is cut; one 50 ms
    /// into it is inside and is peeked.
    ///
    /// Non-vacuity: time the peek with a fresh `timeout(idle, …)` instead of
    /// the opening's deadline and the 150 ms event lands inside a new gap, so
    /// this goes red.
    #[tokio::test(start_paused = true)]
    async fn peek_first_event_ends_at_the_gap_the_opening_started() {
        async fn peek_after(
            event_at: std::time::Duration,
        ) -> Result<super::BufferedEvent, crate::adapters::AdapterError> {
            let idle = std::time::Duration::from_millis(300);
            let opened_at = tokio::time::Instant::now();
            // The handshake and the frame, before the peek starts.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            tokio::spawn(async move {
                tokio::time::sleep(event_at).await;
                let event = super::ResponseEvent {
                    event: Some("response.created".to_string()),
                    data: Value::Null,
                };
                let _ = tx.send(Ok(event)).await;
            });
            super::peek_first_event(rx, Some((idle, opened_at)))
                .await
                .map(|(first, _, _)| first)
        }

        let error = peek_after(std::time::Duration::from_millis(150))
            .await
            .expect_err("an event past the gap since the opening is cut");
        assert_eq!(
            error
                .response
                .extensions()
                .get::<crate::adapters::UpstreamBodyIdle>(),
            Some(&crate::adapters::UpstreamBodyIdle {
                idle: std::time::Duration::from_millis(300)
            }),
            "got: {}",
            error.message
        );
        let first = peek_after(std::time::Duration::from_millis(50))
            .await
            .expect("an event inside the gap since the opening is peeked");
        assert!(matches!(first, Some(Ok(_))));
    }

    /// Peek `data` as the first event of a turn through `commit_or_fallback`.
    fn commit_first(data: Value) -> Result<super::BufferedEvent, crate::adapters::AdapterError> {
        let (_tx, rx) = tokio::sync::mpsc::channel(16);
        let event = super::ResponseEvent {
            event: data["type"].as_str().map(str::to_string),
            data,
        };
        super::commit_or_fallback(Some(Ok(event)), rx, super::AuthMode::ChatgptOauth)
            .map(|(buffered, _events)| buffered)
    }

    /// A first-event wrapped error frame carrying a non-2xx `status` is re-shaped
    /// like a refused handshake: the upstream status, `UpstreamStatus` failure,
    /// and the frame's `error.message` — not a committed stream that ends in 502.
    #[tokio::test]
    async fn commit_or_fallback_reshapes_a_wrapped_status_error_frame() {
        let message =
            "The 'gpt-6-luna' model is not supported when using Codex with a ChatGPT account.";
        let error = commit_first(json!({
            "type": "error",
            "status": 400,
            "error": {"type": "invalid_request_error", "message": message}
        }))
        .expect_err("a wrapped 400 does not commit");
        assert_eq!(error.response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            error.failure,
            Some(crate::adapters::AdapterFailure::UpstreamStatus(
                StatusCode::BAD_REQUEST
            ))
        );
        let body = axum::body::to_bytes(error.response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert_eq!(body["error"]["message"], message);
    }

    /// The frame's `headers` object carries `retry-after` (any case, string or
    /// number) onto the re-shaped response, and `status_code` is read too.
    #[test]
    fn commit_or_fallback_carries_a_wrapped_frame_retry_after() {
        for (data, expected) in [
            (
                json!({"type": "error", "status": 429, "headers": {"retry-after": "30"},
                    "error": {"message": "slow"}}),
                "30",
            ),
            (
                json!({"type": "error", "status_code": 429, "headers": {"Retry-After": 7},
                    "error": {"message": "slow"}}),
                "7",
            ),
        ] {
            let error = commit_first(data.clone()).expect_err("a wrapped 429 does not commit");
            assert_eq!(
                error.response.status(),
                StatusCode::TOO_MANY_REQUESTS,
                "{data}"
            );
            assert_eq!(error.response.headers()["retry-after"], expected, "{data}");
        }
    }

    /// `websocket_connection_limit_reached` is retryable upstream: it falls back
    /// to HTTP as a pre-header failure, whatever status accompanies it.
    #[test]
    fn commit_or_fallback_falls_back_on_the_connection_limit() {
        let error = commit_first(json!({
            "type": "error",
            "status": 429,
            "error": {"code": "websocket_connection_limit_reached", "message": "limit"}
        }))
        .expect_err("the connection limit does not commit");
        assert_eq!(
            error.failure,
            Some(crate::adapters::AdapterFailure::BeforeHeaders)
        );
    }

    /// An error frame without a usable status (absent, 2xx, or non-numeric) is
    /// an ordinary backend error event: it commits and streams, as before.
    #[test]
    fn commit_or_fallback_commits_an_error_frame_without_a_status() {
        for data in [
            json!({"type": "error", "error": {"code": "server_error", "message": "x"}}),
            json!({"type": "error", "status": 200, "error": {"message": "x"}}),
            json!({"type": "error", "status": "400", "error": {"message": "x"}}),
        ] {
            let buffered = commit_first(data.clone()).expect("commits");
            assert!(matches!(buffered, Some(Ok(_))), "{data}");
        }
    }

    #[test]
    fn websocket_headers_send_bearer_only_for_non_codex_credentials() {
        use super::codex_ws::WEBSOCKET_BETA_PROTOCOL;
        use super::{websocket_headers, Credential};

        // Every non-ChatGPT credential still sends its bearer (a misconfiguration
        // fails upstream, not silently unauthenticated) plus the beta protocol,
        // but never the ChatGPT/Codex account-id or identity headers.
        let cases = [
            Credential::ApiKey {
                value: "api-key".to_string(),
                header: crate::config::ApiKeyHeader::Bearer,
            },
            Credential::XaiOauth {
                access_token: "xai-tok".to_string(),
            },
            Credential::CursorOauth {
                access_token: "cursor-tok".to_string(),
            },
            Credential::ClaudeOauth {
                access_token: "claude-tok".to_string(),
                account_uuid: None,
            },
        ];
        for credential in cases {
            let headers = websocket_headers(credential, Some(&hint()), Some("sess-1"), 0, None)
                .expect("valid credential builds headers");
            assert_eq!(headers.get("openai-beta").unwrap(), WEBSOCKET_BETA_PROTOCOL);
            assert!(headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("Bearer "));
            assert!(headers.get("chatgpt-account-id").is_none());
            assert!(headers.get("originator").is_none());
            assert!(headers.get("session-id").is_none());
            assert!(headers.get("thread-id").is_none());
            assert!(headers.get("x-client-request-id").is_none());
            assert!(headers.get("x-codex-window-id").is_none());
            // Upstream suppresses the routing hint for api-key/bearer providers.
            assert!(headers.get("x-codex-routing-hint").is_none());
        }
    }

    #[test]
    fn websocket_headers_carry_the_routing_hint_on_the_chatgpt_oauth_arm() {
        use super::{websocket_headers, Credential};

        // The handshake mirrors the HTTP request's `x-codex-routing-hint`
        // verbatim — the value is built once per connection from the route.
        let headers = websocket_headers(
            Credential::ChatGptOAuth {
                access_token: "access-token".to_string(),
                account_id: "account-id".to_string(),
            },
            Some(&axum::http::HeaderValue::from_static(
                "model=gpt-5.6-sol;tier=priority",
            )),
            Some("session-123"),
            0,
            None,
        )
        .expect("valid credential builds headers");
        assert_eq!(
            headers.get("x-codex-routing-hint").unwrap(),
            "model=gpt-5.6-sol;tier=priority"
        );
        assert_eq!(headers.get("session-id").unwrap(), "session-123");
        assert_eq!(headers.get("thread-id").unwrap(), "session-123");
        assert_eq!(headers.get("x-client-request-id").unwrap(), "session-123");
        assert_eq!(headers.get("x-codex-window-id").unwrap(), "session-123:0");
    }

    #[test]
    fn websocket_handshake_omits_the_hint_for_an_unusable_client_model() {
        use super::super::request::MAX_ROUTING_HINT_MODEL_LEN;
        use super::{routing_hint, websocket_headers, Credential, Route};

        // Composes exactly as `forward_websocket` does: `routing_hint(route)`
        // feeding the handshake builder. A prefix/default route puts the
        // client's raw `model` into `upstream_model`, so each of these is a
        // reachable request body.
        let over_long = "g".repeat(MAX_ROUTING_HINT_MODEL_LEN + 1);
        let bad_models = [
            // One character outside the allowlist each, so widening it by a
            // single byte reddens exactly one case (a full forge like
            // `gpt-5,tier=priority` is rejected by its `=` first, which would
            // hide the `,` rule).
            "gpt-5;codex",
            "gpt-5,codex",
            "gpt-5=codex",
            "gpt-5 codex",
            "gpt-5\tcodex",
            // not a valid header value at all
            "gpt-5\ncodex",
            // unbounded client model
            over_long.as_str(),
            // reachable: `strip_context_window_hint("[1m]") == ""`
            "",
        ];
        for model in bad_models {
            let route = Route {
                model: model.to_string(),
                upstream_model: model.to_string(),
                provider: "codex".to_string(),
                adapter: crate::routing::AdapterKind::Responses,
                effort: None,
                service_tier: None,
            };
            let headers = websocket_headers(
                Credential::ChatGptOAuth {
                    access_token: "access-token".to_string(),
                    account_id: "account-id".to_string(),
                },
                routing_hint(&route).as_ref(),
                None,
                0,
                None,
            )
            .expect("an unusable model must not fail the handshake build");
            assert!(
                headers.get("x-codex-routing-hint").is_none(),
                "model {model:?} must not produce a routing hint"
            );
            // Only the hint is dropped; the rest of the identity still goes out.
            assert_eq!(headers.get("originator").unwrap(), "codex_cli_rs");
        }
    }

    #[test]
    fn websocket_handshake_carries_the_advanced_window_id() {
        use super::{websocket_headers, Credential};

        let headers = websocket_headers(
            Credential::ChatGptOAuth {
                access_token: "access-token".to_string(),
                account_id: "account-id".to_string(),
            },
            None,
            Some("session-123"),
            1,
            None,
        )
        .expect("valid credential builds headers");
        assert_eq!(headers.get("x-codex-window-id").unwrap(), "session-123:1");
    }

    #[test]
    fn websocket_handshake_carries_the_delegated_subagent_markers() {
        use super::{websocket_headers, Credential};
        use crate::adapters::responses::request::CodexDelegation;

        let headers = websocket_headers(
            Credential::ChatGptOAuth {
                access_token: "access-token".to_string(),
                account_id: "account-id".to_string(),
            },
            None,
            Some("session-123"),
            0,
            Some(&CodexDelegation {
                thread_id: "session-123::agent-7".to_string(),
                parent_thread_id: "session-123".to_string(),
                subagent: "Explore".to_string(),
                agent_id: "agent-7".to_string(),
            }),
        )
        .expect("valid credential builds headers");
        assert_eq!(headers.get("session-id").unwrap(), "session-123");
        assert_eq!(headers.get("thread-id").unwrap(), "session-123::agent-7");
        assert_eq!(
            headers.get("x-codex-parent-thread-id").unwrap(),
            "session-123"
        );
        assert_eq!(headers.get("x-openai-subagent").unwrap(), "Explore");
        // Every thread-derived id — request id and window id included —
        // carries the child identity; only `session-id` stays the parent's.
        assert_eq!(
            headers.get("x-client-request-id").unwrap(),
            "session-123::agent-7"
        );
        assert_eq!(
            headers.get("x-codex-window-id").unwrap(),
            "session-123::agent-7:0"
        );
    }

    #[test]
    fn websocket_headers_omit_the_session_headers_for_an_empty_session_id() {
        use super::{websocket_headers, Credential};

        let headers = websocket_headers(
            Credential::ChatGptOAuth {
                access_token: "access-token".to_string(),
                account_id: "account-id".to_string(),
            },
            Some(&hint()),
            Some(""),
            0,
            None,
        )
        .expect("valid credential builds headers");
        assert!(headers.get("session-id").is_none());
        assert!(headers.get("thread-id").is_none());
        assert!(headers.get("x-client-request-id").is_none());
        assert!(headers.get("x-codex-window-id").is_none());
        assert_eq!(headers.get("originator").unwrap(), "codex_cli_rs");
    }

    #[test]
    fn websocket_headers_passthrough_sends_only_the_beta_protocol() {
        use super::codex_ws::WEBSOCKET_BETA_PROTOCOL;
        use super::{websocket_headers, Credential};

        // Passthrough is a misconfiguration on this transport: no credential is
        // attached, leaving the upstream to reject it.
        let headers =
            websocket_headers(Credential::Passthrough, Some(&hint()), None, 0, None).unwrap();
        assert_eq!(headers.get("openai-beta").unwrap(), WEBSOCKET_BETA_PROTOCOL);
        assert!(headers.get("authorization").is_none());
        assert!(headers.get("x-codex-routing-hint").is_none());
    }

    #[test]
    fn websocket_headers_antigravity_oauth_sends_no_authorization() {
        use super::codex_ws::WEBSOCKET_BETA_PROTOCOL;
        use super::{websocket_headers, Credential};

        // Config validation pins `antigravity_oauth` to `kind = "antigravity"`,
        // so this arm is unreachable in a valid config — but if it were ever
        // reached, it must fail closed rather than bearer an off-origin
        // subscription token onto the Codex WebSocket.
        let headers = websocket_headers(
            Credential::AntigravityOauth {
                access_token: "antigravity-token".to_string(),
                project_id: "proj-1".to_string(),
            },
            Some(&hint()),
            None,
            0,
            None,
        )
        .unwrap();
        assert_eq!(headers.get("openai-beta").unwrap(), WEBSOCKET_BETA_PROTOCOL);
        assert!(headers.get("authorization").is_none());
        assert!(headers.get("x-api-key").is_none());
        assert!(headers.get("x-codex-routing-hint").is_none());
    }

    #[test]
    fn websocket_headers_reject_malformed_credential_as_bad_gateway() {
        use super::{websocket_headers, Credential};

        // An account-id with a control character cannot be a header value; the
        // builder returns a pre-header 502 gateway error rather than panicking.
        let error = websocket_headers(
            Credential::ChatGptOAuth {
                access_token: "ok".to_string(),
                account_id: "bad\nid".to_string(),
            },
            Some(&hint()),
            None,
            0,
            None,
        )
        .expect_err("a malformed header value is rejected");
        assert_eq!(error.response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            error.failure,
            Some(crate::adapters::AdapterFailure::BeforeHeaders)
        );
    }

    #[test]
    fn ws_connect_error_maps_transport_failure_without_status_to_bad_gateway() {
        use super::{ws_connect_error, AuthMode, CodexWsError};

        // A pure transport failure (no HTTP status: DNS, TLS, timeout) maps to
        // 502, like a failed HTTP send.
        let error = ws_connect_error(
            CodexWsError {
                status: None,
                retry_after: None,
                body: String::new(),
                message: "dns failure".to_string(),
                previous_response_missing: false,
            },
            AuthMode::ChatgptOauth,
        );
        assert_eq!(error.response.status(), StatusCode::BAD_GATEWAY);
    }
}
