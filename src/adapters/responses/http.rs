//! The HTTP Responses transport: send the request, then relay the upstream
//! answer to the client as Anthropic SSE or a single JSON body. The default
//! path for every provider and the fallback when the websocket transport fails
//! to connect (see [`super::forward`]).

use axum::{
    body::{Body, Bytes},
    http::{Response, StatusCode},
    response::IntoResponse,
};
use futures_util::{stream, StreamExt};

use crate::{
    adapters::AdapterError,
    auth::Credential,
    model::responses::{parse_sse_events, ResponseEvent},
    routing::Route,
    server::AppState,
};

use super::context::{ForwardOptions, RelayOptions};
use super::error::{backend_error_response, mapped_upstream_error, own_error, transport_error};
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
) -> Result<reqwest::Response, reqwest::Error> {
    let mut request = request_builder(state, route, credential, session_id);
    if body.zstd {
        // Exactly what the Codex CLI sends alongside a compressed Responses
        // request body (codex-rs/http-client/src/request.rs).
        request = request.header("content-encoding", "zstd");
    }
    request.body(body.bytes).send().await
}

/// The upstream request body for one HTTP send: the bytes to put on the wire and
/// whether they are zstd-compressed (and therefore need `content-encoding: zstd`).
/// Cloning is a refcount bump, so every retry attempt, account rotation, and
/// refresh retry reuses one preparation (see [`prepare_body`]).
#[derive(Debug, Clone)]
pub(super) struct PreparedBody {
    bytes: bytes::Bytes,
    zstd: bool,
}

impl PreparedBody {
    fn plain(bytes: bytes::Bytes) -> Self {
        Self { bytes, zstd: false }
    }
}

/// Serialize the translated request and, on the ChatGPT/Codex backend,
/// zstd-compress it (issue #285) — the same wire shape the Codex CLI sends. A
/// long agentic turn re-uploads its whole history every request, and JSON of that
/// shape compresses several times over, so the saving grows with the conversation.
///
/// Called at most once per turn, before the bounded-retry and account-rotation
/// loops, so neither a retry nor a rotation repeats the work (the same
/// serialize-once discipline as issue #251). A compression failure is not fatal:
/// the uncompressed body is always acceptable to the backend, so it is logged and
/// sent as-is rather than failing the turn.
pub(super) async fn prepare_body(
    state: &AppState,
    route: &Route,
    upstream_body: &serde_json::Value,
) -> PreparedBody {
    // `to_vec` serializes straight into the byte buffer `Bytes` takes ownership
    // of, skipping the `fmt` machinery and UTF-8 round-trip `Value::to_string()`
    // pays for. Serializing a `Value` cannot fail, so the fallback is
    // unreachable — it just keeps the old path.
    let bytes = serde_json::to_vec(upstream_body)
        .map(bytes::Bytes::from)
        .unwrap_or_else(|_| bytes::Bytes::from(upstream_body.to_string()));
    if !state.config.responses_request_compression(&route.provider) {
        return PreparedBody::plain(bytes);
    }
    match crate::compression::compress_request_body(bytes.clone()).await {
        Ok(Some(compressed)) => {
            tracing::debug!(
                provider = %route.provider,
                body_bytes = bytes.len(),
                compressed_bytes = compressed.len(),
                "compressed responses request body"
            );
            PreparedBody {
                bytes: compressed,
                zstd: true,
            }
        }
        // Below the size where compression pays for itself.
        Ok(None) => PreparedBody::plain(bytes),
        Err(error) => {
            tracing::warn!(
                provider = %route.provider,
                body_bytes = bytes.len(),
                error = %error,
                "failed to compress responses request body; sending it uncompressed"
            );
            PreparedBody::plain(bytes)
        }
    }
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
    session_id: Option<&str>,
) -> Result<(StatusCode, axum::response::Response), AdapterError> {
    let ForwardOptions {
        upstream_body,
        credential,
        auth,
        turn,
        codex_quota_account,
        estimate_input,
    } = forward;
    // Kick off the CPU-bound tiktoken encode on the blocking pool *before* the
    // upstream request so it overlaps that round-trip; the result is not needed
    // until the response stream (and thus message_start) begins. `None` on
    // non-streaming turns and non-tiktoken providers (gated in `forward`).
    let estimate_handle = estimate_input.map(|request| {
        tokio::task::spawn_blocking(move || crate::count_tokens::count_input_tokens_value(&request))
    });
    // The account-pool path drives its own failover and deliberately does not
    // layer retry on top. This single-credential path retries only before any
    // response body is handed to the streaming/JSON relay.
    let policy = provider_retry_policy(state, route);
    let body = prepare_body(state, route, upstream_body.as_ref()).await;
    let upstream = crate::retry::send_with_retry_with_safety(
        policy,
        &route.provider,
        crate::retry::RetrySafety::NonIdempotentPost,
        || http_send(state, route, credential.clone(), session_id, body.clone()),
    )
    .await
    .map_err(|error| {
        // Preserve the raw transport cause in logs before transport_error maps it to
        // the stable gateway-facing Responses error envelope.
        tracing::warn!(
            provider = %route.provider,
            error = %error,
            "responses upstream request failed after retries"
        );
        transport_error(error.to_string())
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
    if turn.client_wants_stream {
        let input_tokens_estimate = match estimate_handle {
            Some(handle) => handle.await.unwrap_or(0),
            None => 0,
        };
        let keepalive = std::time::Duration::from_secs(state.config.server.sse_keepalive_seconds);
        Ok((
            StatusCode::OK,
            stream_response(
                upstream,
                turn.relay(route),
                input_tokens_estimate,
                keepalive,
            ),
        ))
    } else {
        // Thread the real response status: `json_response` returns a `502` when
        // a backend error event surfaced via `backend_error` (issue #113), so
        // the proxy's access log (`upstream_status`) and `record_proxied_request`
        // metrics reflect the failure instead of a hardcoded `200`.
        let response = json_response(upstream, turn.relay(route)).await?;
        Ok((response.status(), response))
    }
}

pub(super) fn stream_response(
    upstream: reqwest::Response,
    relay: RelayOptions,
    input_tokens_estimate: u64,
    keepalive: std::time::Duration,
) -> axum::response::Response {
    let bytes = upstream.bytes_stream();
    let parser = SseParser::default();
    let machine = relay
        .machine()
        .with_input_estimate(input_tokens_estimate)
        .without_content_accumulation();
    let output = stream::unfold((bytes, parser, machine, false), |state| async move {
        let (mut bytes, mut parser, mut machine, mut finished) = state;
        if finished {
            return None;
        }
        loop {
            match bytes.next().await {
                Some(Ok(chunk)) => {
                    let events = parser.push(&chunk);
                    let data = events
                        .into_iter()
                        .flat_map(|event| machine.apply(event))
                        .collect::<String>();
                    if !data.is_empty() {
                        return Some((
                            Ok::<_, reqwest::Error>(Bytes::from(data)),
                            (bytes, parser, machine, false),
                        ));
                    }
                }
                Some(Err(error)) => return Some((Err(error), (bytes, parser, machine, true))),
                None => {
                    let data = machine.finish().join("");
                    finished = true;
                    if data.is_empty() {
                        return None;
                    }
                    return Some((Ok(Bytes::from(data)), (bytes, parser, machine, finished)));
                }
            }
        }
    });

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
pub(super) async fn json_response(
    upstream: reqwest::Response,
    relay: RelayOptions,
) -> Result<axum::response::Response, AdapterError> {
    let body = upstream
        .text()
        .await
        .map_err(|error| own_error(format!("failed to read Responses body: {error}")))?;
    let mut machine = relay.machine();
    for event in parse_sse_events(&body) {
        let _ = machine.apply(event);
    }
    if let Some(error) = machine.take_backend_error() {
        return Err(AdapterError {
            message: "responses backend error event".into(),
            response: Box::new(backend_error_response(error)),
            failure: None,
        });
    }
    Ok((StatusCode::OK, axum::Json(machine.final_json())).into_response())
}

/// Frame-buffers the upstream SSE byte stream. Buffering raw bytes — rather than
/// decoding each transport chunk with `from_utf8_lossy` — keeps a multi-byte
/// UTF-8 code point intact when it straddles a chunk boundary: the incomplete
/// trailing bytes stay in the buffer until the next chunk completes them. Frame
/// boundaries are the ASCII `\n\n`, which can never fall inside a multi-byte
/// sequence, so every extracted frame is already complete UTF-8.
#[derive(Default)]
struct SseParser {
    buffer: Vec<u8>,
    scan_from: usize,
}

impl SseParser {
    fn push(&mut self, chunk: &[u8]) -> Vec<ResponseEvent> {
        self.buffer.extend_from_slice(chunk);

        let mut complete_end = None;
        let mut scan = self.scan_from;
        while scan + 1 < self.buffer.len() {
            if self.buffer[scan] == b'\n' && self.buffer[scan + 1] == b'\n' {
                complete_end = Some(scan + 2);
                scan += 2;
            } else {
                scan += 1;
            }
        }

        let Some(complete_end) = complete_end else {
            // The final byte may be the first half of a frame terminator, so scan
            // it again after the next chunk arrives. Everything before it has
            // already been ruled out.
            self.scan_from = self.buffer.len().saturating_sub(1);
            return Vec::new();
        };

        // Parse all complete frames in one UTF-8 decode, then compact the buffer
        // once. Front-draining each frame shifts the same trailing bytes over and
        // over when one transport chunk contains many SSE events.
        let out = parse_sse_events(&String::from_utf8_lossy(&self.buffer[..complete_end]));
        self.buffer.drain(..complete_end);
        self.scan_from = self.buffer.len().saturating_sub(1);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use serde_json::Value;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The default relay options for these tests: the `gpt-5.2-codex` model with
    /// both protocol toggles off.
    fn relay_opts() -> RelayOptions {
        RelayOptions {
            model: "gpt-5.2-codex".to_string(),
            thinking_enabled: false,
            tool_search_native: false,
        }
    }

    /// A translated request body large enough to be worth compressing (a real
    /// turn's instructions and history are far larger still).
    fn upstream_body() -> Value {
        serde_json::json!({
            "model": "gpt-5.2-codex",
            "instructions": "be brief".repeat(200),
            "input": [{"role": "user", "content": "hello"}],
            "stream": true,
        })
    }

    /// Point `provider`'s base_url at a mock server and return the state to send
    /// through it.
    fn state_for(provider: &str, base_url: String) -> AppState {
        let mut config = crate::config::Config::default();
        config
            .providers
            .get_mut(provider)
            .expect("built-in provider should exist")
            .base_url = base_url;
        AppState::new(config, reqwest::Client::new()).expect("state should build")
    }

    fn route_for(provider: &str) -> Route {
        Route {
            provider: provider.to_string(),
            adapter: crate::routing::AdapterKind::Responses,
            model: "gpt-5.2-codex".to_string(),
            upstream_model: "gpt-5.2-codex".to_string(),
            effort: None,
        }
    }

    /// Serve a `200` from `path` and return the single request the mock recorded.
    async fn record_send(
        provider: &str,
        endpoint: &str,
        credential: Credential,
    ) -> wiremock::Request {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_string(String::new()))
            .mount(&server)
            .await;
        let state = state_for(provider, server.uri());
        let route = route_for(provider);
        let body = prepare_body(&state, &route, &upstream_body()).await;
        http_send(&state, &route, credential, None, body)
            .await
            .expect("mock request should succeed");
        server
            .received_requests()
            .await
            .expect("mock server should record requests")
            .pop()
            .expect("exactly one request should have been sent")
    }

    /// The ChatGPT/Codex backend gets the same wire shape real Codex sends: a
    /// zstd-compressed body announced with `content-encoding: zstd` (issue #285).
    #[tokio::test]
    async fn compresses_the_request_body_on_the_chatgpt_backend() {
        let request = record_send(
            "codex",
            "/codex/responses",
            Credential::ChatGptOAuth {
                access_token: "access-token".to_string(),
                account_id: "account-id".to_string(),
            },
        )
        .await;

        assert_eq!(
            request
                .headers
                .get("content-encoding")
                .expect("a compressed body must announce its encoding"),
            "zstd"
        );
        let plain = serde_json::to_vec(&upstream_body()).unwrap();
        assert!(
            request.body.len() < plain.len(),
            "compressed body ({}) should be smaller than the JSON ({})",
            request.body.len(),
            plain.len()
        );
        // The upstream must be able to recover the exact translated request.
        let decoded = crate::compression::decode_zstd_within(
            bytes::Bytes::from(request.body.clone()),
            plain.len(),
        )
        .await
        .expect("the sent body should be valid zstd")
        .expect("the decoded body should fit its own size");
        assert_eq!(decoded, plain);
    }

    /// No other flavor has been verified to accept a compressed request body, so
    /// a stock OpenAI provider keeps sending plain JSON and no encoding header.
    #[tokio::test]
    async fn leaves_the_request_body_uncompressed_on_other_flavors() {
        let request = record_send(
            "openai",
            "/responses",
            Credential::ApiKey {
                value: "sk-test".to_string(),
                header: crate::config::ApiKeyHeader::Bearer,
            },
        )
        .await;

        assert!(request.headers.get("content-encoding").is_none());
        assert_eq!(request.body, serde_json::to_vec(&upstream_body()).unwrap());
    }

    /// The per-provider opt-out returns the ChatGPT path to plain JSON.
    #[tokio::test]
    async fn honors_the_per_provider_opt_out() {
        let mut config = crate::config::Config::default();
        config
            .providers
            .get_mut("codex")
            .expect("built-in provider should exist")
            .request_compression = false;
        let state = AppState::new(config, reqwest::Client::new()).expect("state should build");

        let body = prepare_body(&state, &route_for("codex"), &upstream_body()).await;
        assert!(!body.zstd);
        assert_eq!(body.bytes, serde_json::to_vec(&upstream_body()).unwrap());
    }

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
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"Rate limit reached\"}}}\n\n",
        );
        let upstream = upstream_response(200, sse).await;
        let error = json_response(upstream, relay_opts())
            .await
            .expect_err("backend error event should stop failover");

        assert!(error.failure.is_none());
        assert_eq!(error.response.status(), StatusCode::BAD_GATEWAY);
        let body = response_body_json(*error.response).await;
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["message"], "Rate limit reached");
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
        let response = json_response(upstream, relay_opts())
            .await
            .expect("json_response builds a response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = response_body_json(response).await;
        assert_eq!(body["type"], "message");
        assert_eq!(body["content"][0]["text"], "hello");
    }

    /// A multi-byte code point split across two transport chunks must survive
    /// intact. Decoding each chunk with `from_utf8_lossy` in isolation would
    /// replace the straddling bytes with U+FFFD; buffering raw bytes until a
    /// frame boundary keeps the text whole.
    #[test]
    fn sse_parser_preserves_multibyte_char_split_across_chunks() {
        let frame = "event: delta\ndata: {\"text\":\"안녕\"}\n\n";
        // Split one byte into the 3-byte '녕' so the first chunk ends
        // mid-code-point.
        let split = frame.find('녕').unwrap() + 1;
        let (head, tail) = frame.as_bytes().split_at(split);

        let mut parser = SseParser::default();
        // No frame boundary yet, and the incomplete byte must be held back
        // rather than decoded and corrupted.
        assert!(parser.push(head).is_empty());

        let events = parser.push(tail);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("delta"));
        assert_eq!(events[0].data["text"], "안녕");
    }

    /// A completed frame followed by an incomplete frame is emitted immediately,
    /// while the trailing bytes remain buffered and are not rescanned from the
    /// beginning when the next chunk arrives.
    #[test]
    fn sse_parser_retains_an_incomplete_trailing_frame() {
        let mut parser = SseParser::default();
        let events = parser.push(b"event: a\ndata: {\"n\":1}\n\nevent: b\ndata: {\"n\":");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data["n"], 1);

        let events = parser.push(b"2}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("b"));
        assert_eq!(events[0].data["n"], 2);
    }

    /// A frame terminator split across chunks is detected by rescanning the
    /// previous chunk's final byte.
    #[test]
    fn sse_parser_detects_terminator_split_across_chunks() {
        let mut parser = SseParser::default();
        assert!(parser.push(b"event: a\ndata: {\"n\":1}\n").is_empty());

        let events = parser.push(b"\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data["n"], 1);
    }

    /// A frame that arrives split at an arbitrary ASCII byte still parses once
    /// the terminator lands, and only completed frames are emitted per push.
    #[test]
    fn sse_parser_emits_only_completed_frames() {
        let mut parser = SseParser::default();
        assert!(parser.push(b"event: a\ndata: {\"n\":1}\n").is_empty());
        let events = parser.push(b"\nevent: b\ndata: {\"n\":2}\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data["n"], 1);
        assert_eq!(events[1].data["n"], 2);
    }
}
