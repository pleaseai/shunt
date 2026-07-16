//! The Codex Responses WebSocket v2 transport (issue #32): reuse the session's
//! pooled connection, send only the continuation delta when the input is an
//! append-only extension, and peek the first event so a pre-first-token failure
//! can transparently fall back to HTTP.

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
use super::error::{build_upstream_error, own_error};
use super::request::{responses_url, CODEX_CLIENT_VERSION, CODEX_USER_AGENT};
use super::ws_stream::{json_events_response, stream_events_response};

/// Drive a turn over the Codex Responses WebSocket v2 transport (issue #32).
/// Reuses the session's pooled connection and, when the current input is an
/// append-only extension of the previous turn, sends only the delta with
/// `previous_response_id` (the payload-reduction lever). Events are re-encoded
/// through the same [`AnthropicSseMachine`] the HTTP path uses; a rejected
/// handshake is re-shaped exactly like an HTTP upstream error.
pub(super) async fn forward_websocket(
    state: &AppState,
    route: &Route,
    pool_key: Option<&str>,
    forward: ForwardOptions,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let ForwardOptions {
        upstream_body,
        credential,
        auth,
        turn,
        codex_quota_account,
        estimate_input,
    } = forward;
    let pool_key = pool_key.filter(|key| !key.is_empty());
    let http_url = responses_url(&state.config, &route.provider);
    let ws_url = codex_ws::to_websocket_url(&http_url).map_err(ws_transport_error)?;
    let ctx = WsTurnContext {
        ws_url,
        pool_key,
        provider: &route.provider,
        accounts: &state.accounts,
        codex_quota_account: codex_quota_account.as_deref(),
        credential,
        auth,
        signature: codex_continuation::signature(&upstream_body),
        full_input: upstream_body
            .get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        upstream_body,
    };
    tracing::debug!(provider = %route.provider, ws_url = %ctx.ws_url, pool_key = pool_key.unwrap_or(""), "opening codex websocket");

    // Overlap the CPU-bound tiktoken encode with the websocket connect (same
    // rationale as forward_http); its result is only consumed once the event
    // stream begins. If open_ws_turn fails, this handle is dropped un-awaited and
    // forward_http re-spawns its own encode on the fallback path — a rare,
    // off-executor double-encode we accept rather than thread the handle back out
    // through the Err path (which would couple the transport signatures).
    let estimate_handle = estimate_input.map(|request| {
        tokio::task::spawn_blocking(move || crate::count_tokens::count_input_tokens_value(&request))
    });
    let (buffered, events) = open_ws_turn(&ctx).await?;
    if turn.client_wants_stream {
        let input_tokens_estimate = match estimate_handle {
            Some(handle) => handle.await.unwrap_or(0),
            None => 0,
        };
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
        let response = json_events_response(buffered, events, turn.relay(route)).await;
        Ok((response.status(), response))
    }
}

/// Everything needed to (re)start a websocket turn for a request.
struct WsTurnContext<'a> {
    ws_url: String,
    pool_key: Option<&'a str>,
    provider: &'a str,
    accounts: &'a crate::accounts::AccountPool,
    codex_quota_account: Option<&'a str>,
    credential: Credential,
    auth: AuthMode,
    signature: String,
    full_input: Vec<Value>,
    upstream_body: Value,
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
async fn open_ws_turn(
    ctx: &WsTurnContext<'_>,
) -> Result<(BufferedEvent, CodexWsEvents), AdapterError> {
    let (events, used_continuation) = start_ws_turn(ctx, true).await?;
    let (first, events) = peek_first_event(events).await;
    // A rejected previous_response_id arrives before any output: retry once with
    // the full input on a fresh connection, then evaluate that stream instead.
    if used_continuation && matches!(&first, Some(Err(error)) if error.previous_response_missing) {
        tracing::info!("codex previous_response_id rejected; retrying with full input");
        let (events, _) = start_ws_turn(ctx, false).await?;
        let (first, events) = peek_first_event(events).await;
        return commit_or_fallback(first, events);
    }
    commit_or_fallback(first, events)
}

/// Await the first event of a freshly opened turn, returning it alongside the
/// still-live channel so it can be replayed before the remainder of the stream.
async fn peek_first_event(mut events: CodexWsEvents) -> (BufferedEvent, CodexWsEvents) {
    let first = events.recv().await;
    (first, events)
}

/// Decide, from the peeked first event, whether to commit to the websocket
/// response or fall back to HTTP. A delivered first event (`Ok`) means the turn is
/// under way: buffer it for replay and stream the socket. A transport error or an
/// empty stream means nothing ever reached the client, so return `Err` to let
/// [`forward`] re-drive the turn over HTTP transparently — the send→first-event
/// analogue of the pre-handshake fallback. Backend-sent error *events* (a rate
/// limit, a content-policy refusal) arrive as `Ok` and are streamed through rather
/// than retried; only genuine transport failures reach the `Err` arm here.
fn commit_or_fallback(
    first: BufferedEvent,
    events: CodexWsEvents,
) -> Result<(BufferedEvent, CodexWsEvents), AdapterError> {
    match first {
        Some(Ok(event)) => Ok((Some(Ok(event)), events)),
        Some(Err(error)) => Err(ws_transport_error(error)),
        None => Err(own_error(
            "codex websocket closed before any event".to_string(),
        )),
    }
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
    let headers = websocket_headers(ctx.credential.clone())?;
    let turn = codex_ws::begin(&ctx.ws_url, headers, ctx.pool_key)
        .await
        .map_err(|error| ws_connect_error(error, ctx.auth))?;
    // Only a fresh connection carries new handshake headers. Reused/prewarmed
    // sockets do not handshake again, so dashboard usage refreshes the next time
    // this account establishes a new connection.
    if let (Some(account), Some(headers)) = (ctx.codex_quota_account, turn.handshake_headers()) {
        ctx.accounts
            .note_codex_quota(ctx.provider, account, headers);
    }

    let mut frame_body = ctx.upstream_body.clone();
    let mut used_continuation = false;
    if allow_continuation {
        // Only a reused connection carries stored continuation state; a fresh
        // connection has no chance to continue and is not counted, so the hit/
        // fallback series stay directly comparable (issue #45).
        if let Some(stored) = turn.stored_continuation() {
            match codex_continuation::decide(&stored, &ctx.upstream_body) {
                Some(decision) => {
                    let turn_state = stored
                        .turn_state
                        .as_deref()
                        .or_else(|| turn.handshake_turn_state());
                    let delta_items = apply_continuation(&mut frame_body, &decision, turn_state);
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

    let frame = codex_ws::response_create_frame(frame_body);
    let record = codex_ws::RecordPlan {
        signature: ctx.signature.clone(),
        request_input: ctx.full_input.clone(),
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
fn websocket_headers(credential: Credential) -> Result<HeaderMap, AdapterError> {
    let mut headers = HeaderMap::new();
    let mut set = |name: &'static str, value: String| -> Result<(), AdapterError> {
        let value = HeaderValue::from_str(&value).map_err(|error| {
            let message = format!("invalid {name} header: {error}");
            let response = ShuntError::bad_gateway(message.clone()).into_response();
            AdapterError {
                message,
                response: Box::new(response),
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
        Credential::Passthrough => {}
    }
    Ok(headers)
}

fn ws_transport_error(error: CodexWsError) -> AdapterError {
    let response = ShuntError::bad_gateway(error.message.clone()).into_response();
    AdapterError {
        message: error.message,
        response: Box::new(response),
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
        use super::{commit_or_fallback, CodexWsError, ResponseEvent};
        use tokio::sync::mpsc;

        // A delivered first event commits: it is buffered for replay and the
        // channel is handed back intact.
        let (_tx, rx) = mpsc::channel(16);
        let event = ResponseEvent {
            event: Some("response.created".to_string()),
            data: Value::Null,
        };
        let (buffered, _events) =
            commit_or_fallback(Some(Ok(event)), rx).expect("a delivered event commits");
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
            commit_or_fallback(Some(Err(error)), rx).is_err(),
            "a pre-first-event transport error falls back to HTTP"
        );

        // An empty stream (channel closed before any event) also falls back.
        let (_tx, rx) = mpsc::channel(16);
        assert!(
            commit_or_fallback(None, rx).is_err(),
            "an empty stream falls back to HTTP"
        );
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
            let headers = websocket_headers(credential).expect("valid credential builds headers");
            assert_eq!(headers.get("openai-beta").unwrap(), WEBSOCKET_BETA_PROTOCOL);
            assert!(headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("Bearer "));
            assert!(headers.get("chatgpt-account-id").is_none());
            assert!(headers.get("originator").is_none());
        }
    }

    #[test]
    fn websocket_headers_passthrough_sends_only_the_beta_protocol() {
        use super::codex_ws::WEBSOCKET_BETA_PROTOCOL;
        use super::{websocket_headers, Credential};

        // Passthrough is a misconfiguration on this transport: no credential is
        // attached, leaving the upstream to reject it.
        let headers = websocket_headers(Credential::Passthrough).unwrap();
        assert_eq!(headers.get("openai-beta").unwrap(), WEBSOCKET_BETA_PROTOCOL);
        assert!(headers.get("authorization").is_none());
    }

    #[test]
    fn websocket_headers_reject_malformed_credential_as_bad_gateway() {
        use super::{websocket_headers, Credential};

        // An account-id with a control character cannot be a header value; the
        // builder returns a 502 gateway error rather than panicking.
        let error = websocket_headers(Credential::ChatGptOAuth {
            access_token: "ok".to_string(),
            account_id: "bad\nid".to_string(),
        })
        .expect_err("a malformed header value is rejected");
        assert_eq!(error.response.status(), StatusCode::BAD_GATEWAY);
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
