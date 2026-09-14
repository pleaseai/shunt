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
    let response = early_streaming_response(machine, std::time::Duration::from_secs(30), failing);
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
    let response = early_streaming_response(machine, std::time::Duration::from_secs(30), events);
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
    assert!(parser.push(head).0.is_empty());

    let (events, malformed) = parser.push(tail);
    assert!(!malformed);
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
    let (events, _) = parser.push(b"event: a\ndata: {\"n\":1}\n\nevent: b\ndata: {\"n\":");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["n"], 1);

    let (events, _) = parser.push(b"2}\n\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.as_deref(), Some("b"));
    assert_eq!(events[0].data["n"], 2);
}

/// A frame terminator split across chunks is detected by rescanning the
/// previous chunk's final byte.
#[test]
fn sse_parser_detects_terminator_split_across_chunks() {
    let mut parser = SseParser::default();
    assert!(parser.push(b"event: a\ndata: {\"n\":1}\n").0.is_empty());

    let (events, _) = parser.push(b"\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["n"], 1);
}

/// A frame that arrives split at an arbitrary ASCII byte still parses once
/// the terminator lands, and only completed frames are emitted per push.
#[test]
fn sse_parser_emits_only_completed_frames() {
    let mut parser = SseParser::default();
    assert!(parser.push(b"event: a\ndata: {\"n\":1}\n").0.is_empty());
    let (events, _) = parser.push(b"\nevent: b\ndata: {\"n\":2}\n\n");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].data["n"], 1);
    assert_eq!(events[1].data["n"], 2);
}

/// CRLF framing is legal SSE: every frame must parse, or a CRLF upstream
/// hands the client only the synthetic completion and none of its output.
#[test]
fn sse_parser_parses_crlf_framed_events() {
    let mut parser = SseParser::default();
    let (events, malformed) =
        parser.push(b"event: a\r\ndata: {\"n\":1}\r\n\r\nevent: b\r\ndata: {\"n\":2}\r\n\r\n");
    assert!(!malformed);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].data["n"], 1);
    assert_eq!(events[1].data["n"], 2);
}

/// The four-byte CRLF terminator split across chunks is found the same way
/// the two-byte LF one is.
#[test]
fn sse_parser_detects_crlf_terminator_split_across_chunks() {
    let mut parser = SseParser::default();
    assert!(parser
        .push(b"event: a\r\ndata: {\"n\":1}\r\n\r")
        .0
        .is_empty());

    let (events, _) = parser.push(b"\n");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["n"], 1);
}

/// A complete frame whose data is not valid JSON flags the stream instead
/// of being dropped: the client must not receive a synthesized completion
/// over corrupted upstream data. Events completed before the bad frame
/// still come through.
#[test]
fn sse_parser_flags_complete_frame_with_invalid_json() {
    let mut parser = SseParser::default();
    let (events, malformed) = parser.push(b"event: a\ndata: {\"n\":1}\n\ndata: not-json\n\n");
    assert!(malformed);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["n"], 1);
}

/// Comment-only and `[DONE]` frames are not data: they never flag the
/// stream.
#[test]
fn sse_parser_ignores_non_data_frames_without_flagging() {
    let mut parser = SseParser::default();
    let (events, malformed) = parser.push(b": comment\n\ndata: [DONE]\n\n");
    assert!(!malformed);
    assert!(events.is_empty());
}

/// A 2xx upstream whose complete frame carries invalid JSON ends the
/// stream with one terminal `error` event carrying the gateway envelope —
/// the turn is not silently completed without the upstream output. (The
/// post-error fuse itself is pinned deterministically in
/// `parsed_events_ends_after_a_malformed_frame`, which controls its own
/// chunk boundaries.)
#[tokio::test]
async fn http_events_stream_turns_a_malformed_frame_into_a_terminal_error() {
    let sse = concat!(
        "event: response.created\n",
        "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
        "data: not-json\n\n",
        "event: response.output_text.delta\n",
        "data: {\"delta\":\"must never arrive\"}\n\n",
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
    assert_eq!(collected.len(), 2, "the good event, then one terminal item");
    let first = collected[0]
        .as_ref()
        .expect("first item is the parsed event");
    assert_eq!(first.event.as_deref(), Some("response.created"));
    let Err(envelope) = &collected[1] else {
        panic!("expected an error envelope, got {:?}", collected[1]);
    };
    assert_eq!(envelope["error"]["type"], "api_error");
    assert_eq!(
        envelope["error"]["message"],
        "upstream sent an SSE frame whose data is not valid JSON"
    );
}

/// `parsed_events` is terminal after its error item: a consumer polling past
/// it must not receive events that arrived after the bad frame. The trailing
/// good frame rides in its own chunk, so an unfused stream would parse and
/// relay it — the fused one never reads past the error.
#[tokio::test]
async fn parsed_events_ends_after_a_malformed_frame() {
    use futures_util::StreamExt;
    let chunks = vec![
        Ok::<_, reqwest::Error>(axum::body::Bytes::from(
            "event: a\ndata: {\"n\":1}\n\ndata: not-json\n\n",
        )),
        Ok::<_, reqwest::Error>(axum::body::Bytes::from("event: b\ndata: {\"n\":2}\n\n")),
    ];
    let collected: Vec<_> = parsed_events(futures_util::stream::iter(chunks))
        .collect()
        .await;
    assert_eq!(collected.len(), 2, "the good event, then one terminal item");
    assert_eq!(collected[0].as_ref().expect("event is ok").data["n"], 1);
    assert!(collected[1].is_err(), "got: {:?}", collected[1]);
}

/// A slow estimator never delays the committed start: the budget elapses and
/// the seed falls back to `0`.
#[tokio::test]
async fn bounded_input_estimate_falls_back_after_the_budget() {
    let handle = tokio::task::spawn_blocking(|| {
        std::thread::sleep(std::time::Duration::from_millis(200));
        42u64
    });
    let estimate = bounded_input_estimate(handle, std::time::Duration::from_millis(10)).await;
    assert_eq!(estimate, 0);
}

/// A fast estimator's value still lands.
#[tokio::test]
async fn bounded_input_estimate_keeps_the_value_when_it_lands_in_time() {
    let handle = tokio::task::spawn_blocking(|| 7u64);
    let estimate = bounded_input_estimate(handle, std::time::Duration::from_secs(1)).await;
    assert_eq!(estimate, 7);
}

/// A transport error reaches the client without the upstream URL: the error
/// envelope's message must not embed the request URL, matching the redaction
/// convention used by every other client-visible transport error in the
/// adapter. The send fails against port 0, which can never accept a
/// connection — unlike a released ephemeral port, which another process could
/// rebind between the drop and the request.
#[tokio::test]
async fn http_events_stream_redacts_the_upstream_url_from_transport_errors() {
    let mut config = crate::config::Config::default();
    let upstream_url = "http://127.0.0.1:0".to_string();
    config.providers.get_mut("codex").unwrap().base_url = upstream_url.clone();
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
    assert_eq!(envelope["error"]["type"], "api_error");
    let message = envelope["error"]["message"]
        .as_str()
        .expect("message is a string");
    assert!(
        !message.contains(&upstream_url),
        "upstream URL leaked into the client-visible message: {message}"
    );
}

/// A CRLF-framed upstream relays every event — only the LF terminator was
/// recognized before, which would have dropped the whole body.
#[tokio::test]
async fn http_events_stream_relays_crlf_framed_upstream() {
    let sse = concat!(
        "event: response.created\r\n",
        "data: {\"response\":{\"id\":\"resp_1\"}}\r\n\r\n",
        "event: response.output_text.delta\r\n",
        "data: {\"delta\":\"hi\"}\r\n\r\n",
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
        vec!["response.created", "response.output_text.delta"]
    );
}
