use super::*;
use crate::model::responses::AnthropicSseMachine;
use crate::proxy::chain_stream::LazyEnvelope;
use axum::body::to_bytes;
use axum::http::StatusCode;
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
    let response = early_streaming_response(
        move || {
            Box::pin(async move {
                let mut machine = machine;
                let start = machine.start_synthetic(format!("msg_{}", uuid::Uuid::new_v4()));
                (machine, start.join(""))
            })
        },
        std::time::Duration::from_secs(30),
        never,
    );
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

/// The producer's first poll overlaps the machine build: the upstream
/// dispatch starts while the factory still waits (the bounded estimate)
/// instead of serializing behind it, and the item the producer produced in
/// the meantime relays after the synthetic start.
#[tokio::test]
async fn first_producer_poll_overlaps_the_machine_build() {
    use futures_util::StreamExt;
    let (build_release, build_wait) = tokio::sync::oneshot::channel::<()>();
    let (polled_tx, polled_rx) = tokio::sync::oneshot::channel::<()>();
    let machine = relay_opts().machine().without_content_accumulation();
    let events = futures_util::stream::unfold(Some(polled_tx), |tx| async move {
        let tx = tx?;
        let _ = tx.send(());
        Some((
            Ok(ResponseEvent {
                event: Some("response.output_text.delta".to_string()),
                data: json!({"delta": "after"}),
            }),
            None,
        ))
    });
    let response = early_streaming_response(
        move || {
            Box::pin(async move {
                let _ = build_wait.await;
                let mut machine = machine;
                let start = machine.start_synthetic(format!("msg_{}", uuid::Uuid::new_v4()));
                (machine, start.join(""))
            })
        },
        std::time::Duration::from_secs(30),
        events,
    );
    let mut body = response.into_body().into_data_stream();
    let first_poll = tokio::spawn(async move {
        let item = body.next().await;
        (item, body)
    });
    // The producer must be polled while the build is still blocked: a
    // serialized build never reaches the producer, so the signal never
    // arrives and the timeout fails the test.
    tokio::time::timeout(std::time::Duration::from_secs(2), polled_rx)
        .await
        .expect("the producer's first poll overlaps the machine build")
        .expect("the poll signal is sent");
    build_release
        .send(())
        .expect("the factory is still listening");
    let (first, mut body) = tokio::time::timeout(std::time::Duration::from_secs(2), first_poll)
        .await
        .expect("the synthetic start arrives once the build is released")
        .expect("the body task joined");
    let first = first.expect("the stream yields").expect("the chunk is ok");
    let text = String::from_utf8(first.to_vec()).expect("chunk is utf8");
    assert!(
        text.starts_with("event: message_start\ndata: "),
        "got: {text}"
    );
    // The item the producer produced during the build relays after the start.
    let second = body
        .next()
        .await
        .expect("the stream yields")
        .expect("the chunk is ok");
    let second = String::from_utf8(second.to_vec()).expect("chunk is utf8");
    assert!(second.contains("\"text\":\"after\""), "got: {second}");
}

/// The pooled chain attempt's machine build (the bounded estimate) overlaps
/// the pool's first poll: the pool is polled while the build is still
/// blocked, the first item is buffered, and the pending build then resolves
/// for the winner arm — a serialized build never reaches the poll, so the
/// signal never arrives and the timeout fails the test.
#[tokio::test]
async fn pooled_first_poll_overlaps_the_machine_build() {
    let (build_release, build_wait) = tokio::sync::oneshot::channel::<()>();
    let (polled_tx, polled_rx) = tokio::sync::oneshot::channel::<()>();
    let events = futures_util::stream::unfold(Some(polled_tx), |tx| async move {
        let tx = tx?;
        let _ = tx.send(());
        Some((
            Ok(PoolItem::Event(PoolEvent::Account("pool-a".to_string()))),
            None,
        ))
    });
    let machine = relay_opts().machine().without_content_accumulation();
    let resolved_at = std::sync::Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
    let build_resolved_at = resolved_at.clone();
    let build = async move {
        let _ = build_wait.await;
        *build_resolved_at
            .lock()
            .expect("the build holds the only lock") = Some(std::time::Instant::now());
        let mut machine = machine;
        let start = machine.start_synthetic(format!("msg_{}", uuid::Uuid::new_v4()));
        (machine, start)
    };
    let poll = tokio::spawn(pooled_first_poll(Box::pin(events), build));
    tokio::time::timeout(std::time::Duration::from_secs(2), polled_rx)
        .await
        .expect("the pool's first poll overlaps the machine build")
        .expect("the poll signal is sent");
    build_release
        .send(())
        .expect("the build is still listening");
    let poll = poll.await.expect("the first poll task joined");
    match poll.item {
        Some(Ok(PoolItem::Event(PoolEvent::Account(name)))) => assert_eq!(name, "pool-a"),
        _ => panic!("first item is the account frame"),
    }
    let (machine, start) = match poll.build {
        MachineBuild::Pending(build) => build.await,
        MachineBuild::Ready(..) => panic!("the build was still blocked when the pool won the race"),
    };
    // headers_at is the item's arrival: captured before the still-pending
    // build resolved, never after the winner arm awaited it.
    let resolved = resolved_at
        .lock()
        .expect("the build holds the only lock")
        .expect("the build ran");
    assert!(
        poll.headers_at <= resolved,
        "headers_at {:?} was captured after the build resolved at {:?}",
        poll.headers_at,
        resolved
    );
    assert!(
        start.join("").contains("event: message_start"),
        "the winner arm gets the synthetic start, got: {}",
        start.join("")
    );
    let _ = machine;
}

/// A pre-frame pool failure never waits on the machine build: the exhausted
/// item classifies the attempt while the build is still pending, and the
/// pending build is dropped with it — a serialized build never returns, so
/// the timeout fails the test.
#[tokio::test]
async fn pooled_first_poll_never_waits_for_the_build_on_a_pre_frame_failure() {
    let events = futures_util::stream::iter([Ok(PoolItem::Exhausted {
        status: StatusCode::BAD_GATEWAY,
        advance: true,
        remember: false,
        envelope: LazyEnvelope::Ready(Value::Null),
    })]);
    let build = async move {
        let (machine, start): (AnthropicSseMachine, Vec<String>) =
            futures_util::future::pending().await;
        (machine, start)
    };
    let poll = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        pooled_first_poll(Box::pin(events), build),
    )
    .await
    .expect("a pre-frame failure classifies without waiting on the build");
    assert!(matches!(poll.item, Some(Ok(PoolItem::Exhausted { .. }))));
    assert!(
        matches!(poll.build, MachineBuild::Pending(_)),
        "the build was never awaited"
    );
}

/// When the machine build wins the race, the first item still arrives and
/// the resolved build carries the synthetic start.
#[tokio::test]
async fn pooled_first_poll_returns_the_item_after_the_build_wins() {
    let events = futures_util::stream::iter([Ok(PoolItem::Event(PoolEvent::Account(
        "pool-a".to_string(),
    )))]);
    let machine = relay_opts().machine().without_content_accumulation();
    let build = async move {
        let mut machine = machine;
        let start = machine.start_synthetic(format!("msg_{}", uuid::Uuid::new_v4()));
        (machine, start)
    };
    let poll = pooled_first_poll(Box::pin(events), build).await;
    assert!(matches!(poll.build, MachineBuild::Ready(..)));
    assert!(matches!(
        poll.item,
        Some(Ok(PoolItem::Event(PoolEvent::Account(_))))
    ));
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
    let response = early_streaming_response(
        move || {
            Box::pin(async move {
                let mut machine = machine;
                let start = machine.start_synthetic(format!("msg_{}", uuid::Uuid::new_v4()));
                (machine, start.join(""))
            })
        },
        std::time::Duration::from_secs(30),
        failing,
    );
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
    let response = early_streaming_response(
        move || Box::pin(async move { (machine, String::new()) }),
        std::time::Duration::from_secs(30),
        events,
    );
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
    let upstream_body = std::sync::Arc::new(json!({"input": []}));
    let events = http_events_stream(
        HttpSendContext {
            state,
            route: codex_route(),
            policy: crate::retry::RetryPolicy::DISABLED,
            credential: None,
            session_id: None,
            upstream_body,
            auth: crate::config::AuthMode::ApiKey,
            codex_quota_account: None,
        },
        CredentialSource::Resolved(Credential::ApiKey {
            value: "probe".to_string(),
            header: crate::config::ApiKeyHeader::Bearer,
        }),
        None,
    );
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
    let upstream_body = std::sync::Arc::new(json!({"input": []}));
    let events = http_events_stream(
        HttpSendContext {
            state,
            route: codex_route(),
            policy: crate::retry::RetryPolicy::DISABLED,
            credential: None,
            session_id: None,
            upstream_body,
            auth: crate::config::AuthMode::ApiKey,
            codex_quota_account: None,
        },
        CredentialSource::Resolved(Credential::ApiKey {
            value: "probe".to_string(),
            header: crate::config::ApiKeyHeader::Bearer,
        }),
        None,
    );
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
    let upstream_body = std::sync::Arc::new(json!({"input": []}));
    let events = http_events_stream(
        HttpSendContext {
            state,
            route: codex_route(),
            policy: crate::retry::RetryPolicy::DISABLED,
            credential: None,
            session_id: None,
            upstream_body,
            auth: crate::config::AuthMode::ApiKey,
            codex_quota_account: None,
        },
        CredentialSource::Resolved(Credential::ApiKey {
            value: "probe".to_string(),
            header: crate::config::ApiKeyHeader::Bearer,
        }),
        None,
    );
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
    let upstream_body = std::sync::Arc::new(json!({"input": []}));
    let events = http_events_stream(
        HttpSendContext {
            state,
            route: codex_route(),
            policy: crate::retry::RetryPolicy::DISABLED,
            credential: None,
            session_id: None,
            upstream_body,
            auth: crate::config::AuthMode::ApiKey,
            codex_quota_account: None,
        },
        CredentialSource::Resolved(Credential::ApiKey {
            value: "probe".to_string(),
            header: crate::config::ApiKeyHeader::Bearer,
        }),
        None,
    );
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
    let upstream_body = std::sync::Arc::new(json!({"input": []}));
    let events = http_events_stream(
        HttpSendContext {
            state,
            route: codex_route(),
            policy: crate::retry::RetryPolicy::DISABLED,
            credential: None,
            session_id: None,
            upstream_body,
            auth: crate::config::AuthMode::ApiKey,
            codex_quota_account: None,
        },
        CredentialSource::Resolved(Credential::ApiKey {
            value: "probe".to_string(),
            header: crate::config::ApiKeyHeader::Bearer,
        }),
        None,
    );
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

/// An advance-worthy status carries its error body unread: the chain must
/// advance on the status alone, never wait on a slow or non-terminating
/// upstream error body (the pre-commit loop's lazy-body rule).
#[tokio::test]
async fn send_classified_defers_the_error_body_for_advance_statuses() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_string("{}"))
        .mount(&server)
        .await;
    let mut config = crate::config::Config::default();
    config.providers.get_mut("codex").unwrap().base_url = server.uri();
    let state = AppState::new(config, reqwest::Client::new()).unwrap();
    let context = HttpSendContext {
        state,
        route: codex_route(),
        policy: crate::retry::RetryPolicy::DISABLED,
        credential: Some(Credential::ApiKey {
            value: "probe".to_string(),
            header: crate::config::ApiKeyHeader::Bearer,
        }),
        session_id: None,
        upstream_body: std::sync::Arc::new(json!({"input": []})),
        auth: crate::config::AuthMode::ApiKey,
        codex_quota_account: None,
    };
    let outcome = send_classified(&context).await;
    let SendClassified::Failed {
        envelope,
        status,
        remember,
        advance,
    } = outcome
    else {
        panic!("expected a failed classification, got a relay");
    };
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(remember);
    assert!(advance);
    assert!(
        matches!(envelope, LazyEnvelope::Deferred(_)),
        "the error body must stay unread until the chain selects this failure"
    );
}

/// A terminal status builds its envelope eagerly: it is the answer, not a
/// chain candidate.
#[tokio::test]
async fn send_classified_builds_the_envelope_eagerly_for_terminal_statuses() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string("{}"))
        .mount(&server)
        .await;
    let mut config = crate::config::Config::default();
    config.providers.get_mut("codex").unwrap().base_url = server.uri();
    let state = AppState::new(config, reqwest::Client::new()).unwrap();
    let context = HttpSendContext {
        state,
        route: codex_route(),
        policy: crate::retry::RetryPolicy::DISABLED,
        credential: Some(Credential::ApiKey {
            value: "probe".to_string(),
            header: crate::config::ApiKeyHeader::Bearer,
        }),
        session_id: None,
        upstream_body: std::sync::Arc::new(json!({"input": []})),
        auth: crate::config::AuthMode::ApiKey,
        codex_quota_account: None,
    };
    let outcome = send_classified(&context).await;
    let SendClassified::Failed {
        envelope, status, ..
    } = outcome
    else {
        panic!("expected a failed classification, got a relay");
    };
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        matches!(envelope, LazyEnvelope::Ready(_)),
        "a terminal status is the answer; its envelope must be ready"
    );
}

/// The committed response goes out before a deferred credential resolves: a
/// networked refresh must never starve the client of headers and keepalive
/// pings.
#[tokio::test]
async fn forward_http_commits_before_resolving_the_credential() {
    use crate::adapters::responses::context::{ForwardOptions, TurnOptions};
    let config = crate::config::Config::default();
    let state = AppState::new(config, reqwest::Client::new()).unwrap();
    let route = codex_route();
    let options = ForwardOptions {
        upstream_body: std::sync::Arc::new(json!({"input": []})),
        auth: crate::config::AuthMode::ApiKey,
        turn: TurnOptions {
            client_wants_stream: true,
            thinking_enabled: false,
            tool_search_native: false,
        },
        codex_quota_account: None,
        estimate_input: None,
    };
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        crate::adapters::responses::http::forward_http(
            &state,
            &route,
            options,
            CredentialSource::Deferred(Box::pin(futures_util::future::pending::<
                Result<Credential, crate::adapters::AdapterError>,
            >())),
            None,
        ),
    )
    .await
    .expect("the commit must not wait on the deferred credential");
    let (status, _response) = outcome.expect("the committed response builds");
    assert_eq!(status, StatusCode::OK);
}

/// A credential resolution failure inside the committed stream becomes one
/// terminal SSE `error` item — never a hang, never a pre-commit 502.
#[tokio::test]
async fn http_events_stream_turns_a_credential_resolution_failure_into_a_terminal_error() {
    let config = crate::config::Config::default();
    let state = AppState::new(config, reqwest::Client::new()).unwrap();
    let error = own_error("credential resolution failed".to_string());
    let context = HttpSendContext {
        state,
        route: codex_route(),
        policy: crate::retry::RetryPolicy::DISABLED,
        credential: None,
        session_id: None,
        upstream_body: std::sync::Arc::new(json!({"input": []})),
        auth: crate::config::AuthMode::ApiKey,
        codex_quota_account: None,
    };
    let events = http_events_stream(
        context,
        CredentialSource::Deferred(Box::pin(async move { Err(error) })),
        None,
    );
    use futures_util::StreamExt;
    let collected: Vec<_> = events.collect().await;
    assert_eq!(collected.len(), 1, "one terminal item");
    let Err(envelope) = &collected[0] else {
        panic!("expected an error envelope, got {:?}", collected[0]);
    };
    assert_eq!(envelope["error"]["message"], "credential resolution failed");
}

/// A credential resolution failure inside the chain keeps the credential's
/// own status: the terminal envelope is a 401 authentication error, so the
/// chain must classify the attempt as 401 — a 502 would misreport the
/// emitted envelope in `shunt.requests` and the request span.
#[tokio::test]
async fn chain_attempt_keeps_the_credential_failure_status() {
    let mut config = crate::config::Config::default();
    // Deterministic 401 with no env or store reads: an api-key provider whose
    // key env var is absent.
    let codex = config
        .providers
        .get_mut("codex")
        .expect("built-in codex provider");
    codex.auth = crate::config::AuthMode::ApiKey;
    codex.api_key_env = Some("SHUNT_CHAIN_TEST_API_KEY_MISSING".to_string());
    let state = AppState::new(config, reqwest::Client::new()).unwrap();
    let body = crate::request::RequestBody::parse(b"{\"input\": []}".to_vec()).unwrap();
    let attempt = crate::adapters::responses::chain_attempt(
        &state,
        &codex_route(),
        &axum::http::HeaderMap::new(),
        body,
    )
    .await;
    match attempt {
        crate::proxy::chain_stream::Attempt::Failed { status, .. } => {
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        _ => panic!("expected a failed attempt, got a winner"),
    }
}

/// A batch that ends with an invalid-UTF-8 frame still relays the valid
/// frames that preceded it: the strict decode flags only the bad frame and
/// never drops its valid prefix.
#[test]
fn sse_parser_relays_valid_frames_before_an_invalid_utf8_frame() {
    let mut parser = SseParser::default();
    let (events, malformed) = parser.push(b"event: a\ndata: {\"n\":1}\n\ndata: \xff\xfe\n\n");
    assert!(malformed);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["n"], 1);
}

/// A terminal event ends the relay even when the upstream keeps the
/// connection open: the stream must end at the terminal output instead of
/// polling a still-open upstream forever.
#[tokio::test]
async fn translated_stream_ends_after_a_terminal_event_even_when_more_frames_follow() {
    use futures_util::StreamExt;
    // The upstream sends the terminal event, then a late frame, then stays
    // silent forever.
    let events = futures_util::stream::iter(vec![
        Ok(ResponseEvent {
            event: Some("response.completed".to_string()),
            data: json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}}),
        }),
        Ok(ResponseEvent {
            event: Some("response.output_text.delta".to_string()),
            data: json!({"delta": "late"}),
        }),
    ])
    .chain(futures_util::stream::pending());
    let machine = relay_opts().machine().without_content_accumulation();
    let response = early_streaming_response(
        move || Box::pin(async move { (machine, String::new()) }),
        std::time::Duration::from_secs(30),
        events,
    );
    let bytes = tokio::time::timeout(
        // The relay drains the still-open upstream for up to the terminal
        // drain budget (2 s) before ending, so the timeout must outlast
        // that budget or the drain races it.
        std::time::Duration::from_secs(3),
        to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("the relay must end at the terminal event even with the upstream still open")
    .expect("body is readable");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        !text.contains("late"),
        "frames after the terminal event must not relay, got: {text}"
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
    let upstream_body = std::sync::Arc::new(json!({"input": []}));
    let events = http_events_stream(
        HttpSendContext {
            state,
            route: codex_route(),
            policy: crate::retry::RetryPolicy::DISABLED,
            credential: None,
            session_id: None,
            upstream_body,
            auth: crate::config::AuthMode::ApiKey,
            codex_quota_account: None,
        },
        CredentialSource::Resolved(Credential::ApiKey {
            value: "probe".to_string(),
            header: crate::config::ApiKeyHeader::Bearer,
        }),
        None,
    );
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
