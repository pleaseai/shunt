//! The HTTP Responses transport: for a streaming client, commit the SSE
//! response with a synthetic `message_start` immediately and drive the
//! upstream send inside the stream; for a non-streaming client, send the
//! request and relay the single JSON answer. The default path for every
//! provider and the fallback when the websocket transport fails to connect
//! (see [`super::forward`]).

use axum::{
    body::Body,
    http::{Response, StatusCode},
    response::IntoResponse,
};

use crate::{
    adapters::{
        collect_upstream_sse_body, idle_error, mark_body_broke, too_large_error, AdapterError,
        ResponseBounds, UpstreamBodyError,
    },
    auth::Credential,
    model::responses::{parse_sse_events, AnthropicSseMachine},
    routing::Route,
    server::AppState,
};

use super::body::{prepare_body, PreparedBody};
use super::context::{CredentialSource, ForwardOptions, RelayOptions};
use super::early_stream::{
    bounded_input_estimate, early_streaming_response, estimated_machine_factory,
    http_events_stream, parsed_events, translated_stream, HttpSendContext,
};
use super::error::{backend_error, mapped_upstream_error, own_error, transport_error};
use super::request::request_builder;

/// Send the upstream Responses HTTP request and return the raw response
/// without judging its status. Split out of [`forward_http`] so the account
/// pool path ([`forward_chatgpt_oauth`]) can classify a response for failover
/// before deciding whether to relay, retry, or rotate. Returns the raw
/// `reqwest::Error` so the bounded-retry layer can distinguish transient
/// transport failures from deterministic ones.
pub(super) async fn http_send(
    state: &AppState,
    route: &Route,
    credential: Credential,
    session_id: Option<&str>,
    body: PreparedBody,
) -> Result<reqwest::Response, crate::upstream_timeout::SendError<reqwest::Error>> {
    crate::upstream_timeout::wait(
        state.config.server.timeouts.upstream_ttfb_ms,
        body.attach(request_builder(state, route, credential, session_id))
            .send(),
    )
    .await
}

/// The bounded-retry policy for `route`'s provider (issue #48), or a disabled
/// policy when the provider somehow isn't found (it was validated at routing).
fn provider_retry_policy(state: &AppState, route: &Route) -> crate::retry::RetryPolicy {
    state
        .config
        .provider(&route.provider)
        .map(|provider| provider.retry.policy())
        .unwrap_or(crate::retry::RetryPolicy::DISABLED)
}

/// Drive a turn over the HTTP Responses path. The default transport for every
/// provider, and the fallback when the opt-in websocket transport fails to
/// connect (see [`forward`]).
pub(super) async fn forward_http(
    state: &AppState,
    route: &Route,
    forward: ForwardOptions,
    credential: CredentialSource,
    session_id: Option<&str>,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let ForwardOptions {
        upstream_body,
        auth,
        turn,
        codex_quota_account,
        estimate_input,
        started_at,
    } = forward;
    let policy = provider_retry_policy(state, route);
    if turn.client_wants_stream {
        // Commit the SSE response now, before any upstream byte: the synthetic
        // `message_start` keeps the client's stall watchdog fed while the
        // upstream thinks in silence, and the send (with its bounded retry)
        // runs inside the stream. The estimate must not hold the commit: if
        // the blocking-pool encode has not finished by the time the stream
        // builds its start, the snapshot seeds `0` (the estimate is a
        // best-effort progress figure, the bound exists so a saturated pool
        // cannot stall the first byte).
        let keepalive = std::time::Duration::from_secs(state.config.server.sse_keepalive_seconds);
        let route_for_start = route.clone();
        // The credential resolves inside the committed stream: a refreshable
        // credential's refresh may be networked and is outside the TTFB
        // timeout, so awaiting it before the commit would starve the client
        // of headers and keepalive pings (the watchdog failure this commit
        // exists to prevent).
        let events = http_events_stream(
            HttpSendContext {
                state: state.clone(),
                route: route.clone(),
                policy,
                credential: None,
                session_id: session_id.map(str::to_string),
                upstream_body: upstream_body.clone(),
                auth,
                codex_quota_account: None,
            },
            credential,
            codex_quota_account,
            started_at,
        );
        return Ok((
            StatusCode::OK,
            early_streaming_response(
                estimated_machine_factory(turn, route_for_start, estimate_input),
                keepalive,
                events,
            ),
        ));
    }
    // The account-pool path drives its own failover and deliberately does not
    // layer retry on top. This single-credential path retries only before any
    // response body is handed to the streaming/JSON relay. The streaming arm
    // prepares the body inside the committed stream (`send_classified`), so
    // compression cannot delay the commit; this non-streaming arm has no
    // commit to protect and prepares here.
    let credential = match credential {
        CredentialSource::Resolved(credential) => credential,
        CredentialSource::Deferred(resolve) => resolve.await?,
    };
    let codex_quota_account =
        codex_quota_account.or_else(|| super::codex_quota_account(&credential));
    let body = prepare_body(state, route, upstream_body.as_ref()).await;
    // Spawned before the send, like the pool loop's own estimate handle, so the
    // CPU-bound tiktoken encode overlaps this request's connect/RTT instead of
    // landing on the response's critical path. Only a non-streaming turn
    // carrying `stop_sequences` reaches here with `Some` (see `forward`'s gate).
    let estimate_handle = estimate_input.map(|request| {
        tokio::task::spawn_blocking(move || crate::count_tokens::count_input_tokens_value(&request))
    });
    let upstream = crate::retry::send_with_retry_with_safety(
        policy,
        &route.provider,
        crate::retry::RetrySafety::NonIdempotentPost,
        || http_send(state, route, credential.clone(), session_id, body.clone()),
    )
    .await
    .map_err(|error| {
        error.into_adapter_error(|error| transport_error(error.without_url().to_string()))
    })?;
    if let Some(account) = &codex_quota_account {
        state
            .accounts
            .note_codex_quota(&route.provider, account, upstream.headers());
    }
    let status = upstream.status();
    if !status.is_success() {
        return Err(mapped_upstream_error(status, upstream, auth).await);
    }
    // Bounded like every other path so a saturated blocking pool cannot stall
    // the response (see `bounded_input_estimate`); by now the encode has had the
    // whole upstream round-trip to finish, so this normally resolves instantly.
    let input_tokens_estimate = match estimate_handle {
        Some(handle) => bounded_input_estimate(handle, std::time::Duration::from_secs(1)).await,
        None => 0,
    };
    // Thread the real response status: `json_response` returns a `502` when
    // a backend error event surfaced via `backend_error` (issue #113), so
    // the proxy's access log (`upstream_status`) and `record_proxied_request`
    // metrics reflect the failure instead of a hardcoded `200`.
    let response = json_response(
        upstream,
        turn.relay(route),
        input_tokens_estimate,
        turn.response_bounds,
    )
    .await?;
    Ok((response.status(), response))
}

pub(super) fn stream_response(
    upstream: reqwest::Response,
    relay: RelayOptions,
    input_tokens_estimate: u64,
    keepalive: std::time::Duration,
) -> axum::response::Response {
    let machine = relay
        .machine()
        .with_input_estimate(input_tokens_estimate)
        .without_content_accumulation();
    let output = translated_stream(parsed_events(upstream.bytes_stream()), machine);
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(crate::keepalive::with_pings(
            output, keepalive,
        )))
        .expect("response builder uses valid status and headers")
        .into_response()
}

/// Collect the full HTTP Responses SSE body into a single Anthropic message for
/// a non-streaming client. A backend-sent `error` / `response.failed` event
/// (delivered as a normal event on the `200 OK` stream — rate-limit,
/// content-policy refusal) is surfaced as a gateway error rather than a `200 OK`
/// with the partial content accumulated before it, so the client cannot mistake
/// a backend failure for a truncated-but-successful result (issue #113). This
/// mirrors the streaming path, which emits the same error inline as an SSE
/// `error` event.
///
/// `input_tokens_estimate` seeds the same local prompt count the streaming path
/// puts in `message_start`. It matters here only for an emulated stop sequence:
/// the stop makes the upstream's `response.completed` usage a no-op, and
/// `final_json` falls back to the estimate when no usage was observed, so
/// without it a stopped turn would report `input_tokens: 0` (issue #605). On
/// every other turn the upstream's real usage arrives and overrides it.
pub(super) async fn json_response(
    upstream: reqwest::Response,
    relay: RelayOptions,
    input_tokens_estimate: u64,
    bounds: ResponseBounds,
) -> Result<axum::response::Response, AdapterError> {
    // This is the one Responses path that buffers a whole upstream reply, so
    // it is the one that has to honour `judge_max_response_bytes`. A judge call
    // is forced non-streaming (`routing::serve` strips `stream`), which means
    // every internal call through a `kind = "responses"` target lands here —
    // and reading it with `text()` would let a judge allocate without bound
    // until the deadline instead of failing open at the configured limit.
    // The idle gap bites here for the same reason on a gated call: this read
    // finishes before `run_chain` returns, so an upstream that commits its
    // headers and stalls would otherwise hold the turn until
    // `gated_max_duration_ms`. The body is SSE, so the gap is measured
    // between completed content frames: keep-alive pings alone do not hold a
    // stalled turn open. The default is the client path and stays
    // byte-for-byte what it was.
    let body = match collect_upstream_sse_body(upstream, bounds.max_bytes, bounds.idle).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(UpstreamBodyError::TooLarge(too_large)) => return Err(too_large_error(too_large)),
        Err(UpstreamBodyError::Idle(idle)) => return Err(idle_error(idle)),
        // Only ever a successful reply here: a turn cut before its terminal
        // event, which `routing::serve` reads back through the marker.
        Err(UpstreamBodyError::Transport(error)) => {
            return Err(mark_body_broke(own_error(format!(
                "failed to read Responses body: {error}"
            ))))
        }
    };
    let mut machine = relay.machine().with_input_estimate(input_tokens_estimate);
    for event in parse_sse_events(&body) {
        let _ = machine.apply(event);
    }
    if let Some((status, error)) = machine.take_backend_error() {
        return Err(backend_error(status, error));
    }
    Ok(message_response(&mut machine))
}

/// The `200` a non-streaming collector answers with: the machine's single
/// message, marked [`crate::stream_metrics::UpstreamTruncated`] when the
/// machine reached no terminal event. Shared with the websocket collector
/// (`ws_stream::json_events_response`) so both transports mark alike.
///
/// Read before `final_json`, which finishes an unstopped machine: a turn that
/// reached no terminal event is still returned, as it always was, but marked,
/// the way the streaming path marks its synthesized completion.
pub(super) fn message_response(machine: &mut AnthropicSseMachine) -> axum::response::Response {
    let truncated = !machine.is_stopped();
    let mut response = (StatusCode::OK, axum::Json(machine.final_json())).into_response();
    if truncated {
        response
            .extensions_mut()
            .insert(crate::stream_metrics::UpstreamTruncated);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::super::context::TurnOptions;
    use super::*;
    use axum::body::to_bytes;
    use serde_json::{json, Value};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::super::early_stream::{codex_route, relay_opts};

    /// Serves `body` at `status` from a mock server and returns the resulting
    /// `reqwest::Response`, mirroring the shape `json_response` reads in
    /// production (a response off the wire, not built in-process).
    async fn upstream_response(status: u16, body: &str) -> reqwest::Response {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/e"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body.to_string()))
            .mount(&server)
            .await;
        reqwest::Client::new()
            .get(format!("{}/e", server.uri()))
            .send()
            .await
            .expect("mock request should succeed")
    }

    async fn response_body_json(response: axum::response::Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable");
        serde_json::from_slice(&bytes).expect("response body should be JSON")
    }

    /// A backend-sent `response.failed` event on the HTTP JSON path surfaces as a
    /// `502` gateway error rather than a `200 OK` with the partial content
    /// collected before it (issue #113).
    #[tokio::test]
    async fn json_response_surfaces_backend_error_event_as_gateway_error() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"partial\"}\n\n",
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_error\",\"message\":\"Upstream failed\"}}}\n\n",
        );
        let upstream = upstream_response(200, sse).await;
        let error = json_response(upstream, relay_opts(), 0, ResponseBounds::default())
            .await
            .expect_err("backend error event should stop failover");

        assert!(error.failure.is_none());
        assert_eq!(error.response.status(), StatusCode::BAD_GATEWAY);
        let body = response_body_json(*error.response).await;
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "api_error");
        assert_eq!(body["error"]["message"], "Upstream failed");
    }

    /// An in-stream `rate_limit_exceeded` on the HTTP JSON path is a throttle the
    /// backend delivered on a `200 OK` stream: it must reach the client as `429`
    /// `rate_limit_error` (not a generic `502`) but stay terminal — the upstream
    /// already accepted the turn, so it is never replayed on the next one.
    #[tokio::test]
    async fn json_response_maps_in_stream_rate_limit_to_429_without_failover() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.failed\n",
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"Rate limit reached\"}}}\n\n",
        );
        let upstream = upstream_response(200, sse).await;
        let error = json_response(upstream, relay_opts(), 0, ResponseBounds::default())
            .await
            .expect_err("in-stream rate limit is an error");

        assert!(error.failure.is_none());
        assert_eq!(error.response.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = response_body_json(*error.response).await;
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert_eq!(body["error"]["message"], "Rate limit reached");
    }

    /// A `200` whose body breaks after its first chunk is a turn cut before its
    /// terminal event: the error carries the marker `routing::serve` reads back
    /// to fall a gated turn back, and is otherwise the error it always was.
    #[tokio::test]
    async fn json_response_marks_a_body_broken_after_the_headers() {
        let chunks = futures_util::stream::iter([
            Ok(bytes::Bytes::from_static(
                b"event: response.created\ndata: {\"response\":{\"id\":\"resp_1\"}}\n\n",
            )),
            Err(std::io::Error::other("connection reset")),
        ]);
        let upstream = reqwest::Response::from(
            axum::http::Response::builder()
                .status(200)
                .body(reqwest::Body::wrap_stream(chunks))
                .unwrap(),
        );
        let error = json_response(upstream, relay_opts(), 0, ResponseBounds::default())
            .await
            .expect_err("a broken body is an error");

        assert!(error.failure.is_none());
        assert_eq!(error.response.status(), StatusCode::BAD_GATEWAY);
        assert!(error
            .response
            .extensions()
            .get::<crate::adapters::UpstreamBodyBroke>()
            .is_some());
    }

    /// A `200` SSE reply built in-process whose chunks arrive on the clock: each
    /// `(delay, bytes)` is yielded `delay` after the one before it.
    fn timed_sse_upstream<S>(chunks: S) -> reqwest::Response
    where
        S: futures_util::Stream<Item = (std::time::Duration, &'static [u8])> + Send + 'static,
    {
        use futures_util::StreamExt;
        let chunks = chunks.then(|(delay, chunk)| async move {
            tokio::time::sleep(delay).await;
            Ok::<_, std::io::Error>(bytes::Bytes::from_static(chunk))
        });
        reqwest::Response::from(
            axum::http::Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(reqwest::Body::wrap_stream(chunks))
                .unwrap(),
        )
    }

    /// A gated read's idle gap is measured between completed content frames:
    /// an upstream that commits its headers and then sends nothing but
    /// keep-alives — comment frames and `event: ping` — well inside the gap is
    /// still cut at the gap, not held until the call's wall-clock bound.
    ///
    /// Non-vacuity: refresh the deadline on every chunk in
    /// `collect_bounded_body` and the keep-alives hold the read open forever,
    /// so the outer timeout fires and this goes red.
    #[tokio::test(start_paused = true)]
    async fn json_response_cuts_a_keep_alive_only_body_at_the_idle_gap() {
        let tick = std::time::Duration::from_millis(50);
        let keep_alives: [&'static [u8]; 2] = [b": keep-alive\n\n", b"event: ping\ndata: {}\n\n"];
        let upstream = timed_sse_upstream(futures_util::stream::iter(
            keep_alives
                .into_iter()
                .cycle()
                .map(move |chunk| (tick, chunk)),
        ));
        let idle = std::time::Duration::from_millis(300);
        let started = tokio::time::Instant::now();
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            json_response(
                upstream,
                relay_opts(),
                0,
                ResponseBounds {
                    max_bytes: None,
                    idle: Some(idle),
                },
            ),
        )
        .await
        .expect("keep-alives alone must not hold the read past the idle gap")
        .expect_err("a body that only keeps alive is cut at the idle gap");

        assert_eq!(
            error
                .response
                .extensions()
                .get::<crate::adapters::UpstreamBodyIdle>(),
            Some(&crate::adapters::UpstreamBodyIdle { idle }),
            "got: {}",
            error.message
        );
        assert!(error.failure.is_none());
        assert_eq!(
            started.elapsed(),
            idle,
            "cut at the gap armed after the headers"
        );
    }

    /// A content frame split across two chunks is progress once its second
    /// half completes it: each half arrives inside the gap, the frame they
    /// complete re-arms it, and the turn finishes even though it spans more
    /// than one gap end to end.
    #[tokio::test(start_paused = true)]
    async fn json_response_counts_a_split_content_frame_as_progress() {
        let gap = std::time::Duration::from_millis(200);
        let upstream = timed_sse_upstream(futures_util::stream::iter([
            (
                std::time::Duration::from_millis(50),
                &b"event: response.created\ndata: {\"response\":{\"id\":\"resp_1\"}}\n\nevent: response.output_item.added\ndata: {\"item\":{\"type\":\"message\"}}\n\n"[..],
            ),
            (gap, &b"event: response.output_text.delta\ndata: {\"del"[..]),
            (gap, &b"ta\":\"hello\"}\n\n"[..]),
            (
                gap + gap,
                &b"event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n"[..],
            ),
        ]));
        let response = json_response(
            upstream,
            relay_opts(),
            0,
            ResponseBounds {
                max_bytes: None,
                idle: Some(std::time::Duration::from_millis(500)),
            },
        )
        .await
        .expect("a turn whose frames keep completing inside the gap finishes");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response_body_json(response).await;
        assert_eq!(body["content"][0]["text"], "hello");
    }

    /// A clean turn still returns the collected Anthropic message as `200 OK` —
    /// the backend-error gate must not regress the success path.
    #[tokio::test]
    async fn json_response_returns_ok_for_a_clean_turn() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"item\":{\"type\":\"message\"}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"hello\"}\n\n",
            "event: response.output_text.done\n",
            "data: {}\n\n",
            "event: response.completed\n",
            "data: {\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
        );
        let upstream = upstream_response(200, sse).await;
        let response = json_response(upstream, relay_opts(), 0, ResponseBounds::default())
            .await
            .expect("json_response builds a response");

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .extensions()
                .get::<crate::stream_metrics::UpstreamTruncated>()
                .is_none(),
            "a completed turn must not be marked truncated"
        );
        let body = response_body_json(response).await;
        assert_eq!(body["type"], "message");
        assert_eq!(body["content"][0]["text"], "hello");
    }

    /// An upstream that ends before `response.completed` still gets its
    /// synthesized message, as before, but marked — the JSON counterpart of
    /// the streaming path's truncation marker, which the gated capture reads.
    #[tokio::test]
    async fn json_response_marks_a_message_synthesized_from_a_truncated_upstream() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"item\":{\"type\":\"message\"}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"partial\"}\n\n",
        );
        let upstream = upstream_response(200, sse).await;
        let response = json_response(upstream, relay_opts(), 0, ResponseBounds::default())
            .await
            .expect("json_response builds a response");

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .extensions()
            .get::<crate::stream_metrics::UpstreamTruncated>()
            .is_some());
        let body = response_body_json(response).await;
        assert_eq!(body["content"][0]["text"], "partial");
    }

    /// A non-streaming turn cut short by an emulated stop sequence reports the
    /// seeded local input estimate. The stop makes the upstream's own
    /// `response.completed` usage a no-op, so without the seed this turn would
    /// serialize `input_tokens: 0` for a non-empty prompt (issue #605).
    #[tokio::test]
    async fn json_response_reports_the_input_estimate_for_a_stopped_turn() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"item\":{\"type\":\"message\"}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"keep<<STOP>>drop\"}\n\n",
            "event: response.output_text.done\n",
            "data: {}\n\n",
            "event: response.completed\n",
            "data: {\"response\":{\"usage\":{\"input_tokens\":42,\"output_tokens\":7}}}\n\n",
        );
        let relay = super::super::context::RelayOptions {
            stop_sequences: vec!["<<STOP>>".to_string()],
            ..relay_opts()
        };
        let upstream = upstream_response(200, sse).await;
        let response = json_response(upstream, relay, 11, ResponseBounds::default())
            .await
            .expect("json_response builds a response");

        let body = response_body_json(response).await;
        assert_eq!(body["content"][0]["text"], "keep");
        assert_eq!(body["stop_reason"], "stop_sequence");
        assert_eq!(body["stop_sequence"], "<<STOP>>");
        // The seed, not the upstream's 42: the stop made that usage a no-op.
        assert_eq!(body["usage"]["input_tokens"], 11);
    }

    /// The streaming path prefixes a synthesized completion with
    /// `stream_metrics::UPSTREAM_TRUNCATED_MARKER` when the upstream
    /// connection ends before a real terminal event, so the observer can
    /// still classify the stream as an upstream cut instead of a normal
    /// completion (see that constant's doc comment for the full rationale).
    #[tokio::test]
    async fn stream_response_marks_a_synthesized_completion_from_a_truncated_upstream() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"item\":{\"type\":\"message\"}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"partial\"}\n\n",
        );
        let upstream = upstream_response(200, sse).await;
        let response = stream_response(
            upstream,
            relay_opts(),
            0,
            std::time::Duration::from_secs(30),
        );
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("streamed body should be readable");
        let body = std::str::from_utf8(&bytes).expect("body is utf8");
        let marker = std::str::from_utf8(crate::stream_metrics::UPSTREAM_TRUNCATED_MARKER).unwrap();

        // The marker sits immediately before the synthesized completion
        // `AnthropicSseMachine::finish` builds once the mock body ends
        // without a `response.completed` — not at the very start of the
        // stream, since the real `content_block_delta` already flowed.
        let expected_synthetic_completion = format!("{marker}\n\nevent: content_block_stop");
        assert!(
            body.contains(&expected_synthetic_completion),
            "truncated stream must carry the marker right before the synthesized completion, got: {body}"
        );
        assert!(body.contains("event: message_stop"));
    }

    /// A clean turn that reaches `response.completed` must not carry the
    /// truncation marker — it exists only for the EOF-before-terminal path.
    #[tokio::test]
    async fn stream_response_does_not_mark_a_genuine_completion() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"item\":{\"type\":\"message\"}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"hello\"}\n\n",
            "event: response.output_text.done\n",
            "data: {}\n\n",
            "event: response.completed\n",
            "data: {\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
        );
        let upstream = upstream_response(200, sse).await;
        let response = stream_response(
            upstream,
            relay_opts(),
            0,
            std::time::Duration::from_secs(30),
        );
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("streamed body should be readable");
        let body = std::str::from_utf8(&bytes).expect("body is utf8");
        let marker = std::str::from_utf8(crate::stream_metrics::UPSTREAM_TRUNCATED_MARKER).unwrap();

        assert!(
            !body.contains(marker),
            "clean completion must not carry the marker, got: {body}"
        );
        assert!(body.contains("event: message_stop"));
    }

    /// The streaming arm of `forward_http` commits the first chunk (the
    /// synthetic start) before the upstream has even sent its response headers.
    #[tokio::test]
    async fn forward_http_streaming_commits_first_chunk_before_upstream_headers() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.completed\n",
            "data: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_secs(30))
                    .set_body_string(sse.to_string()),
            )
            .mount(&server)
            .await;
        let mut config = crate::config::Config::default();
        config.providers.get_mut("codex").unwrap().base_url = server.uri();
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        let forward = ForwardOptions {
            upstream_body: std::sync::Arc::new(json!({"input": []})),
            auth: crate::config::AuthMode::ApiKey,
            turn: TurnOptions {
                client_wants_stream: true,
                thinking_enabled: false,
                tool_search_native: false,
                stop_sequences: Vec::new(),
                response_bounds: ResponseBounds::default(),
            },
            codex_quota_account: None,
            estimate_input: None,
            started_at: None,
        };
        let credential = CredentialSource::Resolved(Credential::ApiKey {
            value: "probe".to_string(),
            header: crate::config::ApiKeyHeader::Bearer,
        });
        let (status, response) = forward_http(&state, &codex_route(), forward, credential, None)
            .await
            .expect("forward_http builds the response without upstream headers");
        assert_eq!(status, StatusCode::OK);
        use futures_util::StreamExt;
        let mut body = response.into_body().into_data_stream();
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), body.next())
            .await
            .expect("first chunk arrives while the upstream is still silent")
            .expect("stream yields")
            .expect("chunk is ok");
        let text = String::from_utf8(first.to_vec()).expect("chunk is utf8");
        assert!(
            text.starts_with("event: message_start\ndata: "),
            "got: {text}"
        );
    }

    /// The committed single-account sample starts at the pre-dispatch instant
    /// the caller seeded (the empty-scan fallback after an in-dispatch account
    /// scan): a back-dated start shows up in the recorded latency.
    #[tokio::test]
    async fn forward_http_seeds_the_committed_sample_from_the_pre_dispatch_start() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "event: response.created\ndata: {\"response\":{\"id\":\"resp_1\"}}\n\n\
                 event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
            ))
            .mount(&server)
            .await;
        let mut config = crate::config::Config::default();
        config.providers.insert(
            "forward-http-start-probe".to_string(),
            config
                .providers
                .get("codex")
                .expect("codex provider is built in")
                .clone(),
        );
        config
            .providers
            .get_mut("forward-http-start-probe")
            .expect("just inserted")
            .base_url = server.uri();
        let mut route = codex_route();
        route.provider = "forward-http-start-probe".to_string();
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        let forward = ForwardOptions {
            upstream_body: std::sync::Arc::new(json!({"input": []})),
            auth: crate::config::AuthMode::ApiKey,
            turn: TurnOptions {
                client_wants_stream: true,
                thinking_enabled: false,
                tool_search_native: false,
                stop_sequences: Vec::new(),
                response_bounds: ResponseBounds::default(),
            },
            codex_quota_account: None,
            estimate_input: None,
            started_at: Some(std::time::Instant::now() - std::time::Duration::from_millis(300)),
        };
        let credential = CredentialSource::Resolved(Credential::ApiKey {
            value: "probe".to_string(),
            header: crate::config::ApiKeyHeader::Bearer,
        });
        let (status, response) = forward_http(&state, &route, forward, credential, None)
            .await
            .expect("forward_http builds the committed response");
        assert_eq!(status, StatusCode::OK);
        to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the committed stream completes");
        let (count, latencies) = crate::metrics::proxied_request_samples_for_tests(
            "forward-http-start-probe",
            "gpt-5.2-codex",
            200,
        );
        assert_eq!(count, 1, "one committed sample");
        assert!(
            latencies[0] >= 150.0,
            "the sample must start at the seeded instant: {}ms",
            latencies[0]
        );
    }

    /// A big non-streaming reply is refused at the cap the caller passed to the
    /// adapter, not merely at the one `routing::serve` applies afterwards.
    ///
    /// This drives `Adapter::forward` rather than `json_response` on purpose.
    /// The cap was already correct inside the collector; the defect was that
    /// the Responses adapter took `response_byte_cap` and dropped it on the
    /// floor, so nothing downstream ever saw it. A test that called
    /// `json_response` directly would have passed against the bug.
    ///
    /// Non-streaming because that is what an internal `[models.router]` call
    /// is — `routing::serve` strips `stream` — so this is the shape every
    /// judge call through a `kind = "responses"` target has.
    #[tokio::test]
    async fn the_adapters_byte_cap_reaches_the_buffered_responses_path() {
        use crate::adapters::Adapter;

        // Comfortably past the cap below, and a well-formed SSE turn so the
        // refusal cannot be mistaken for a parse failure.
        let filler = "x".repeat(64 * 1024);
        let sse = format!(
            concat!(
                "event: response.created\n",
                "data: {{\"response\":{{\"id\":\"resp_1\"}}}}\n\n",
                "event: response.completed\n",
                "data: {{\"response\":{{\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}},\"pad\":\"{filler}\"}}\n\n",
            ),
            filler = filler
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse))
            .mount(&server)
            .await;

        let mut config = crate::config::Config::default();
        config.providers.get_mut("codex").unwrap().base_url = server.uri();
        // `None`, not `ApiKey`: this test is about the byte cap, and `ApiKey`
        // would make `AppState::new` demand a credential env var that has
        // nothing to do with what is being asserted.
        config.providers.get_mut("codex").unwrap().auth = crate::config::AuthMode::None;
        let state = AppState::new(config, reqwest::Client::new()).unwrap();

        let uri: axum::http::Uri = "/v1/messages".parse().unwrap();
        let headers = axum::http::HeaderMap::new();
        let body = crate::request::RequestBody::parse(
            serde_json::to_vec(&json!({
                "model": "gpt-5-codex",
                "messages": [{"role": "user", "content": "hi"}],
            }))
            .unwrap(),
        )
        .expect("request body parses");

        let error = super::super::ResponsesAdapter
            .forward(
                state,
                codex_route(),
                &uri,
                &headers,
                body,
                crate::adapters::ResponseBounds {
                    max_bytes: Some(1024),
                    idle: None,
                },
            )
            .await
            .expect_err("a reply past the cap is refused");

        assert!(
            error
                .response
                .extensions()
                .get::<crate::adapters::UpstreamBodyTooLarge>()
                .is_some(),
            "refusal must carry the oversized marker `routing::serve` reads back, \
             so the judge resolves as `oversized` rather than as a failed upstream; \
             got: {}",
            error.message
        );
        assert!(
            error.failure.is_none(),
            "an upstream that answered correctly and merely answered too much \
             must not advance the failover chain"
        );
    }
}
