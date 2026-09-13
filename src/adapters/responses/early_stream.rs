//! The Responses adapter's early-commit streaming machinery: commit the SSE
//! response with a synthetic `message_start` before any upstream byte, drive
//! the upstream feed (send, retry, parsing) inside the stream, and turn every
//! pre-stream failure into one terminal Anthropic SSE `error` event.

use std::convert::Infallible;

use axum::{
    body::{Body, Bytes},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::{stream, Stream, StreamExt};
use serde_json::Value;

use crate::{
    auth::Credential,
    config::AuthMode,
    model::responses::{sse, AnthropicSseMachine, ResponseEvent},
    routing::Route,
    server::AppState,
};

use super::body::PreparedBody;
use super::error::{adapter_error_envelope, mapped_upstream_error, transport_error};
use super::http::http_send;

/// The streaming response for the early-commit transport: emit the synthetic
/// `message_start` + initial ping immediately — before any upstream byte — and
/// then relay the translated events. The keepalive wrapper spans both phases,
/// so the client hop is never silent for longer than `keepalive` even while
/// the upstream thinks in silence.
pub(super) fn early_streaming_response(
    mut machine: AnthropicSseMachine,
    keepalive: std::time::Duration,
    events: impl Stream<Item = Result<ResponseEvent, Value>> + Send + 'static,
) -> axum::response::Response {
    let start = machine.start_synthetic(format!("msg_{}", uuid::Uuid::new_v4()));
    let output = stream::once(async move { Ok::<Bytes, Infallible>(Bytes::from(start.join(""))) })
        .chain(translated_stream(events, machine));
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(crate::keepalive::with_pings(
            output, keepalive,
        )))
        .expect("response builder uses valid status and headers")
        .into_response()
}

type UpstreamBytes = std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

/// Everything [`http_events_stream`] needs to drive one upstream send.
pub(super) struct HttpSendContext {
    pub(super) state: AppState,
    pub(super) route: Route,
    pub(super) policy: crate::retry::RetryPolicy,
    pub(super) credential: Credential,
    pub(super) session_id: Option<String>,
    pub(super) body: PreparedBody,
    pub(super) auth: AuthMode,
    pub(super) codex_quota_account: Option<crate::config::AccountConfig>,
}

/// One streaming turn's upstream feed: drive the bounded-retry send inside the
/// stream (the response is already committed), capture the x-codex-* quota
/// windows, and turn every pre-stream failure — a retry-exhausted transport
/// error, the TTFB timeout, or a non-2xx status — into the same Anthropic
/// error envelope the pre-commit path returned as a JSON body, now emitted as
/// one terminal SSE `error` event by [`early_streaming_response`].
pub(super) fn http_events_stream(
    context: HttpSendContext,
) -> impl Stream<Item = Result<ResponseEvent, Value>> + Send + 'static {
    let HttpSendContext {
        state,
        route,
        policy,
        credential,
        session_id,
        body,
        auth,
        codex_quota_account,
    } = context;
    enum Phase {
        Send,
        Read { bytes: UpstreamBytes },
        Done,
    }
    stream::unfold(
        (
            Phase::Send,
            SseParser::default(),
            std::collections::VecDeque::new(),
        ),
        move |(phase, parser, pending)| {
            let state = state.clone();
            let route = route.clone();
            let credential = credential.clone();
            let body = body.clone();
            let session_id = session_id.clone();
            let codex_quota_account = codex_quota_account.clone();
            async move {
                let mut phase = phase;
                let mut parser = parser;
                let mut pending = pending;
                loop {
                    match phase {
                        Phase::Send => {
                            let outcome = crate::retry::send_with_retry_with_safety(
                                policy,
                                &route.provider,
                                crate::retry::RetrySafety::NonIdempotentPost,
                                || {
                                    http_send(
                                        &state,
                                        &route,
                                        credential.clone(),
                                        session_id.as_deref(),
                                        body.clone(),
                                    )
                                },
                            )
                            .await;
                            let upstream = match outcome {
                                Ok(response) => response,
                                Err(error) => {
                                    let envelope =
                                        adapter_error_envelope(error.into_adapter_error(|error| {
                                            transport_error(error.to_string())
                                        }))
                                        .await;
                                    return Some((Err(envelope), (Phase::Done, parser, pending)));
                                }
                            };
                            if let Some(account) = &codex_quota_account {
                                state.accounts.note_codex_quota(
                                    &route.provider,
                                    account,
                                    upstream.headers(),
                                );
                            }
                            if !upstream.status().is_success() {
                                let envelope = adapter_error_envelope(
                                    mapped_upstream_error(upstream.status(), upstream, auth).await,
                                )
                                .await;
                                return Some((Err(envelope), (Phase::Done, parser, pending)));
                            }
                            let bytes: UpstreamBytes = Box::pin(upstream.bytes_stream());
                            phase = Phase::Read { bytes };
                        }
                        Phase::Read { bytes } => {
                            let mut bytes = bytes;
                            loop {
                                if let Some(event) = pending.pop_front() {
                                    return Some((
                                        Ok(event),
                                        (Phase::Read { bytes }, parser, pending),
                                    ));
                                }
                                match bytes.as_mut().next().await {
                                    Some(Ok(chunk)) => {
                                        pending.extend(parser.push(&chunk));
                                    }
                                    Some(Err(error)) => {
                                        let envelope = adapter_error_envelope(transport_error(
                                            error.to_string(),
                                        ))
                                        .await;
                                        return Some((
                                            Err(envelope),
                                            (Phase::Done, parser, pending),
                                        ));
                                    }
                                    None => return None,
                                }
                            }
                        }
                        Phase::Done => return None,
                    }
                }
            }
        },
    )
}

/// Frame-buffer and parse the upstream SSE byte stream into
/// [`ResponseEvent`]s. A body error becomes an error envelope
/// (`transport_error`), so every producer failure renders as one terminal SSE
/// `error` event instead of an aborted stream.
pub(super) fn parsed_events(
    bytes: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
) -> impl Stream<Item = Result<ResponseEvent, Value>> + Send + 'static {
    stream::unfold(
        (
            Box::pin(bytes),
            SseParser::default(),
            std::collections::VecDeque::new(),
        ),
        |(mut bytes, mut parser, mut pending)| async move {
            loop {
                if let Some(event) = pending.pop_front() {
                    return Some((Ok(event), (bytes, parser, pending)));
                }
                match bytes.next().await {
                    Some(Ok(chunk)) => {
                        pending.extend(parser.push(&chunk));
                    }
                    Some(Err(error)) => {
                        let envelope =
                            adapter_error_envelope(transport_error(error.to_string())).await;
                        return Some((Err(envelope), (bytes, parser, pending)));
                    }
                    None => return None,
                }
            }
        },
    )
}

/// Translate parsed upstream events through the [`AnthropicSseMachine`] into
/// Anthropic SSE bytes. A producer error envelope becomes an SSE `error` event
/// and ends the stream; a producer that ends before a terminal event gets the
/// synthesized completion prefixed with the upstream-cut marker
/// (`stream_metrics::UPSTREAM_TRUNCATED_MARKER`), exactly like the
/// pre-early-commit relay.
pub(super) fn translated_stream(
    events: impl Stream<Item = Result<ResponseEvent, Value>> + Send + 'static,
    machine: AnthropicSseMachine,
) -> impl Stream<Item = Result<Bytes, Infallible>> + Send + 'static {
    stream::unfold(
        (Box::pin(events), machine, false),
        |(mut events, mut machine, mut finished)| async move {
            if finished {
                return None;
            }
            loop {
                match events.next().await {
                    Some(Ok(event)) => {
                        let data = machine.apply(event).into_iter().collect::<String>();
                        if !data.is_empty() {
                            return Some((Ok(Bytes::from(data)), (events, machine, false)));
                        }
                    }
                    Some(Err(envelope)) => {
                        return Some((
                            Ok(Bytes::from(sse("error", &envelope))),
                            (events, machine, true),
                        ));
                    }
                    None => {
                        let data = machine.finish().join("");
                        finished = true;
                        if data.is_empty() {
                            return None;
                        }
                        // `machine.finish()` only produced output here because
                        // the upstream connection ended before a real
                        // terminal/error event (see `AnthropicSseMachine::finish`):
                        // prefix the synthesized completion with an SSE comment
                        // marker so `stream_metrics::observe_response` can still
                        // classify this as an upstream cut instead of a normal
                        // completion. Real clients ignore `:`-prefixed comment
                        // lines per the WHATWG EventSource spec, so the
                        // client-visible stream stays exactly as well-formed as
                        // before.
                        let mut marked = Vec::with_capacity(
                            crate::stream_metrics::UPSTREAM_TRUNCATED_MARKER.len() + 2 + data.len(),
                        );
                        marked.extend_from_slice(crate::stream_metrics::UPSTREAM_TRUNCATED_MARKER);
                        marked.extend_from_slice(b"\n\n");
                        marked.extend_from_slice(data.as_bytes());
                        return Some((Ok(Bytes::from(marked)), (events, machine, finished)));
                    }
                }
            }
        },
    )
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
        let out = crate::model::responses::parse_sse_events(&String::from_utf8_lossy(
            &self.buffer[..complete_end],
        ));
        self.buffer.drain(..complete_end);
        self.scan_from = self.buffer.len().saturating_sub(1);
        out
    }
}

/// The default relay options for these tests: the `gpt-5.2-codex` model with
/// both protocol toggles off.
#[cfg(test)]
pub(super) fn relay_opts() -> super::context::RelayOptions {
    super::context::RelayOptions {
        model: "gpt-5.2-codex".to_string(),
        thinking_enabled: false,
        tool_search_native: false,
    }
}

#[cfg(test)]
pub(super) fn codex_route() -> Route {
    Route {
        provider: "codex".to_string(),
        adapter: crate::routing::AdapterKind::Responses,
        model: "gpt-5.2-codex".to_string(),
        upstream_model: "gpt-5.2-codex".to_string(),
        effort: None,
        service_tier: None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::body::prepare_body;
    use super::*;
    use axum::body::to_bytes;
    use serde_json::{json, Value};
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The synthetic `message_start` + initial ping reach the client before the
    /// upstream has produced anything — the whole point of the early commit.
    #[tokio::test]
    async fn early_streaming_response_emits_synthetic_start_before_any_upstream_event() {
        use futures_util::StreamExt;
        let machine = relay_opts()
            .machine()
            .with_input_estimate(7)
            .without_content_accumulation();
        let never = futures_util::stream::pending::<Result<ResponseEvent, Value>>();
        let response = early_streaming_response(machine, std::time::Duration::from_secs(30), never);
        let mut body = response.into_body().into_data_stream();
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), body.next())
            .await
            .expect("first chunk arrives without any upstream event")
            .expect("stream yields")
            .expect("chunk is ok");
        let text = String::from_utf8(first.to_vec()).expect("chunk is utf8");
        assert!(
            text.starts_with("event: message_start\ndata: "),
            "got: {text}"
        );
        let data: Value = serde_json::from_str(
            text.split_once("data: ")
                .expect("carries a data line")
                .1
                .split("\n\n")
                .next()
                .expect("event frame is terminated"),
        )
        .expect("message_start data is json");
        assert!(
            data["message"]["id"]
                .as_str()
                .expect("message id")
                .starts_with("msg_"),
            "synthetic id, got: {data}"
        );
        assert_eq!(data["message"]["usage"]["input_tokens"], 7);
        assert!(
            text.contains("event: ping\ndata: {\"type\":\"ping\"}"),
            "initial ping rides with the start, got: {text}"
        );
    }

    /// A producer failure after the early start surfaces as an SSE `error` event
    /// and the stream ends — never a synthesized completion.
    #[tokio::test]
    async fn early_streaming_response_emits_error_event_and_ends_on_producer_failure() {
        let machine = relay_opts().machine().without_content_accumulation();
        let failing = futures_util::stream::iter(vec![Err(json!({
            "type": "error",
            "error": {"type": "api_error", "message": "boom"}
        }))]);
        let response =
            early_streaming_response(machine, std::time::Duration::from_secs(30), failing);
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body is readable");
        let text = String::from_utf8_lossy(&bytes);
        assert_eq!(
            text.matches("event: message_start").count(),
            1,
            "got: {text}"
        );
        assert!(text.contains("event: error\ndata: "), "got: {text}");
        assert!(text.contains("\"message\":\"boom\""), "got: {text}");
        assert!(
            !text.contains("event: message_stop"),
            "no synthesized completion after an error, got: {text}"
        );
    }

    /// A producer that ends before a terminal event keeps the truncated-stream
    /// behavior: the synthesized completion carries the upstream-cut marker.
    #[tokio::test]
    async fn early_streaming_response_synthesizes_completion_when_producer_ends_early() {
        let machine = relay_opts().machine().without_content_accumulation();
        let events = futures_util::stream::iter(vec![
            Ok(ResponseEvent {
                event: Some("response.created".to_string()),
                data: json!({"response": {"id": "resp_1"}}),
            }),
            Ok(ResponseEvent {
                event: Some("response.output_text.delta".to_string()),
                data: json!({"delta": "partial"}),
            }),
        ]);
        let response =
            early_streaming_response(machine, std::time::Duration::from_secs(30), events);
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body is readable");
        let text = String::from_utf8_lossy(&bytes);
        let marker =
            std::str::from_utf8(crate::stream_metrics::UPSTREAM_TRUNCATED_MARKER).expect("marker");
        assert_eq!(
            text.matches("event: message_start").count(),
            1,
            "got: {text}"
        );
        assert!(text.contains("\"text\":\"partial\""), "got: {text}");
        assert!(
            text.contains(&format!("{marker}\n\nevent: content_block_stop")),
            "marker precedes the synthesized completion, got: {text}"
        );
        assert!(text.contains("event: message_stop"), "got: {text}");
    }

    /// A non-2xx upstream status on the producer path becomes one error
    /// envelope (mapped through the existing status→type table) and nothing else.
    #[tokio::test]
    async fn http_events_stream_maps_non_success_to_error_envelope() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string("{}"))
            .mount(&server)
            .await;
        let mut config = crate::config::Config::default();
        config.providers.get_mut("codex").unwrap().base_url = server.uri();
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        let body = prepare_body(&state, &codex_route(), &json!({"input": []})).await;
        let events = http_events_stream(HttpSendContext {
            state,
            route: codex_route(),
            policy: crate::retry::RetryPolicy::DISABLED,
            credential: Credential::ApiKey {
                value: "probe".to_string(),
                header: crate::config::ApiKeyHeader::Bearer,
            },
            session_id: None,
            body,
            auth: crate::config::AuthMode::ApiKey,
            codex_quota_account: None,
        });
        use futures_util::StreamExt;
        let collected: Vec<_> = events.collect().await;
        assert_eq!(collected.len(), 1, "one terminal item");
        let Err(envelope) = &collected[0] else {
            panic!("expected an error envelope, got {:?}", collected[0]);
        };
        assert_eq!(envelope["error"]["type"], "rate_limit_error");
    }

    /// The TTFB timeout on the producer path becomes a `timeout_error` envelope
    /// as one terminal error item, not a 504 JSON response.
    #[tokio::test]
    async fn http_events_stream_maps_ttfb_timeout_to_timeout_error_envelope() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(5)))
            .mount(&server)
            .await;
        let mut config = crate::config::Config::default();
        config.providers.get_mut("codex").unwrap().base_url = server.uri();
        config.server.timeouts.upstream_ttfb_ms = 100;
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        let body = prepare_body(&state, &codex_route(), &json!({"input": []})).await;
        let events = http_events_stream(HttpSendContext {
            state,
            route: codex_route(),
            policy: crate::retry::RetryPolicy::DISABLED,
            credential: Credential::ApiKey {
                value: "probe".to_string(),
                header: crate::config::ApiKeyHeader::Bearer,
            },
            session_id: None,
            body,
            auth: crate::config::AuthMode::ApiKey,
            codex_quota_account: None,
        });
        use futures_util::StreamExt;
        let collected: Vec<_> = events.collect().await;
        assert_eq!(collected.len(), 1, "one terminal item");
        let Err(envelope) = &collected[0] else {
            panic!("expected an error envelope, got {:?}", collected[0]);
        };
        assert_eq!(envelope["error"]["type"], "timeout_error");
    }

    /// A 200 SSE upstream yields its parsed events in order, one item each.
    #[tokio::test]
    async fn http_events_stream_yields_parsed_events_from_streaming_upstream() {
        let sse = concat!(
            "event: response.created\n",
            "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"delta\":\"hi\"}\n\n",
            "event: response.completed\n",
            "data: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(sse.to_string()))
            .mount(&server)
            .await;
        let mut config = crate::config::Config::default();
        config.providers.get_mut("codex").unwrap().base_url = server.uri();
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        let body = prepare_body(&state, &codex_route(), &json!({"input": []})).await;
        let events = http_events_stream(HttpSendContext {
            state,
            route: codex_route(),
            policy: crate::retry::RetryPolicy::DISABLED,
            credential: Credential::ApiKey {
                value: "probe".to_string(),
                header: crate::config::ApiKeyHeader::Bearer,
            },
            session_id: None,
            body,
            auth: crate::config::AuthMode::ApiKey,
            codex_quota_account: None,
        });
        use futures_util::StreamExt;
        let collected: Vec<_> = events.collect().await;
        let names: Vec<_> = collected
            .iter()
            .map(|item| {
                item.as_ref()
                    .expect("events are ok")
                    .event
                    .as_deref()
                    .expect("events carry names")
            })
            .collect();
        assert_eq!(
            names,
            vec![
                "response.created",
                "response.output_text.delta",
                "response.completed"
            ]
        );
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
