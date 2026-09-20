use super::*;
use crate::adapters::responses::sse_parse::MachineBuild;
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

/// The non-pooled chain attempt's estimate is polled while its send is
/// blocked: a serialized seam never polls the estimate before the send
/// completes, the poll signal never arrives, and the timeout fails the test.
#[tokio::test]
async fn send_classified_with_estimate_overlaps_the_estimate_with_the_send() {
    let (estimate_polled_tx, estimate_polled_rx) = tokio::sync::oneshot::channel::<()>();
    let estimate = async move {
        let _ = estimate_polled_tx.send(());
        futures_util::future::pending::<u64>().await
    };
    let (send_release, send_wait) = tokio::sync::oneshot::channel::<()>();
    let send = async move {
        let _ = send_wait.await;
        SendClassified::Relay {
            bytes: Box::pin(futures_util::stream::empty()),
        }
    };
    let raced = tokio::spawn(send_classified_with_estimate(send, estimate));
    tokio::time::timeout(std::time::Duration::from_secs(2), estimate_polled_rx)
        .await
        .expect("the estimate is polled while the send is blocked")
        .expect("the poll signal is sent");
    send_release.send(()).expect("the send is still listening");
    let raced = raced.await.expect("the seam task joined");
    assert!(matches!(raced.classified, SendClassified::Relay { .. }));
    assert!(
        matches!(raced.estimate, EstimateBuild::Pending(_)),
        "the send won the race"
    );
}

/// A classified failure never waits on a still-running estimate: the failed
/// send returns while the estimate is pending, and the pending estimate is
/// dropped with the failure — a serialized seam awaits the pending estimate
/// and the timeout fails the test.
#[tokio::test]
async fn send_classified_with_estimate_never_waits_for_the_estimate_on_a_classified_failure() {
    let estimate = futures_util::future::pending::<u64>();
    let send = async {
        SendClassified::Failed {
            envelope: LazyEnvelope::Ready(Value::Null),
            status: StatusCode::BAD_GATEWAY,
            remember: false,
            advance: true,
        }
    };
    let raced = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        send_classified_with_estimate(send, estimate),
    )
    .await
    .expect("a classified failure returns without waiting on the estimate");
    assert!(matches!(raced.classified, SendClassified::Failed { .. }));
    assert!(
        matches!(raced.estimate, EstimateBuild::Pending(_)),
        "the estimate was never awaited"
    );
}

/// When the estimate wins the race, the send's result still lands and the
/// resolved estimate rides along for the winner arm — a serialized seam
/// awaits the blocked send first and the timeout fails the test.
#[tokio::test]
async fn send_classified_with_estimate_returns_the_send_result_after_the_estimate_wins() {
    let (send_release, send_wait) = tokio::sync::oneshot::channel::<()>();
    let estimate = async move {
        send_release.send(()).expect("the send is still listening");
        42u64
    };
    let send = async move {
        let _ = send_wait.await;
        SendClassified::Relay {
            bytes: Box::pin(futures_util::stream::empty()),
        }
    };
    let raced = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        send_classified_with_estimate(send, estimate),
    )
    .await
    .expect("the send result lands after the estimate won the race");
    assert!(matches!(raced.classified, SendClassified::Relay { .. }));
    assert!(matches!(raced.estimate, EstimateBuild::Ready(42)));
}

/// `headers_at` is the send's completion instant: captured while the
/// estimate is still pending, never after the winner arm awaited it.
#[tokio::test]
async fn send_classified_with_estimate_headers_at_precedes_a_pending_estimate() {
    let (estimate_release, estimate_wait) = tokio::sync::oneshot::channel::<()>();
    let resolved_at = std::sync::Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
    let estimate_resolved_at = resolved_at.clone();
    let estimate = async move {
        let _ = estimate_wait.await;
        *estimate_resolved_at
            .lock()
            .expect("the estimate holds the only lock") = Some(std::time::Instant::now());
        42u64
    };
    let send = async {
        SendClassified::Failed {
            envelope: LazyEnvelope::Ready(Value::Null),
            status: StatusCode::BAD_GATEWAY,
            remember: false,
            advance: true,
        }
    };
    let raced = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        send_classified_with_estimate(send, estimate),
    )
    .await
    .expect("the send's failure returns while the estimate is still pending");
    let EstimateBuild::Pending(build) = raced.estimate else {
        panic!("the send won the race");
    };
    estimate_release
        .send(())
        .expect("the estimate is still listening");
    let value = build.await;
    assert_eq!(value, 42);
    let resolved = resolved_at
        .lock()
        .expect("the estimate holds the only lock")
        .expect("the estimate ran");
    assert!(
        raced.headers_at <= resolved,
        "headers_at {:?} was captured after the estimate resolved at {:?}",
        raced.headers_at,
        resolved
    );
}

/// The chain estimate cache starts the compute once and hands every later
/// attempt the same share: a failed attempt drops its racing share without
/// killing the compute, and the next opted-in attempt resumes it instead of
/// launching a second tokenization. A per-attempt cell (the factory re-run
/// on every call) leaves the counter at 2 and fails the test.
#[tokio::test]
async fn chain_estimate_reuses_the_first_attempts_compute_after_its_share_is_dropped() {
    let cache = crate::adapters::responses::ChainEstimate::default();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (release, wait) = tokio::sync::oneshot::channel::<u64>();
    let first = {
        let calls = calls.clone();
        cache
            .get_or_start(move || {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move { wait.await.expect("the compute holds the receiver") }
            })
            .await
    };
    // Start the compute as the seam's race would, then drop the share
    // mid-compute: the send won the race and the attempt failed.
    {
        futures_util::pin_mut!(first);
        assert!(
            futures_util::poll!(&mut first).is_pending(),
            "the compute is still waiting"
        );
    }
    // The next opted-in attempt must resume the pending compute, never start
    // a second one: the factory runs once per chain, not once per attempt.
    let second = {
        let calls = calls.clone();
        cache
            .get_or_start(move || {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move { futures_util::future::pending::<u64>().await }
            })
            .await
    };
    release.send(7).expect("the compute is still listening");
    let value = tokio::time::timeout(std::time::Duration::from_secs(2), second)
        .await
        .expect("the reused compute resolves after the first share was dropped");
    assert_eq!(value, 7);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one compute per chain, not one per attempt"
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

/// A terminal status defers its envelope like an advance-worthy one: the
/// caller samples latency at header arrival, and the body read runs at
/// resolve, right before the terminal frame is emitted.
#[tokio::test]
async fn send_classified_defers_the_envelope_for_terminal_statuses() {
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
        matches!(envelope, LazyEnvelope::Deferred(_)),
        "the terminal status defers its body read so the latency sample lands at header arrival"
    );
    let envelope = envelope.resolve().await;
    assert!(
        envelope.get("error").is_some(),
        "the resolved envelope maps the 400, got {envelope:?}"
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
            stop_sequences: Vec::new(),
            response_byte_cap: None,
        },
        codex_quota_account: None,
        estimate_input: None,
        started_at: None,
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
    let estimate_cache = std::sync::Arc::new(crate::adapters::responses::ChainEstimate::default());
    let attempt = crate::adapters::responses::chain_attempt(
        &state,
        &codex_route(),
        &axum::http::HeaderMap::new(),
        body,
        &estimate_cache,
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
        // The relay ends at the terminal event while the post-terminal drain
        // runs detached; the timeout only guards against a regression that
        // makes the collection hang.
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

/// A route whose provider is `name`, cloned off the built-in codex provider,
/// so tests observing the global test sample store can use a per-test key
/// (tests in this binary run concurrently and share that store).
fn named_codex_route(provider: &str) -> Route {
    Route {
        provider: provider.to_string(),
        ..codex_route()
    }
}

/// A state whose `provider` is a clone of the built-in codex provider with
/// `base_url` repointed, alongside [`named_codex_route`].
fn state_with_provider(provider: &str, base_url: String) -> AppState {
    let mut config = crate::config::Config::default();
    config.providers.insert(
        provider.to_string(),
        config
            .providers
            .get("codex")
            .expect("codex provider is built in")
            .clone(),
    );
    config
        .providers
        .get_mut(provider)
        .expect("just inserted")
        .base_url = base_url;
    AppState::new(config, reqwest::Client::new()).unwrap()
}

/// The early-commit producer records the request sample when the attempt is
/// classified: a success becomes one `200` sample whose latency covers the
/// real upstream round-trip, never the near-zero dispatch/commit time.
#[tokio::test]
async fn http_events_stream_records_the_sample_at_classification() {
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
                .set_delay(std::time::Duration::from_millis(100))
                .set_body_string(sse.to_string()),
        )
        .mount(&server)
        .await;
    let state = state_with_provider("early-metrics-probe", server.uri());
    let events = http_events_stream(
        HttpSendContext {
            state,
            route: named_codex_route("early-metrics-probe"),
            policy: crate::retry::RetryPolicy::DISABLED,
            credential: None,
            session_id: None,
            upstream_body: std::sync::Arc::new(json!({"input": []})),
            auth: crate::config::AuthMode::ApiKey,
            codex_quota_account: None,
        },
        CredentialSource::Resolved(Credential::ApiKey {
            value: "probe".to_string(),
            header: crate::config::ApiKeyHeader::Bearer,
        }),
        None,
        None,
    );
    use futures_util::StreamExt;
    let collected: Vec<_> = events.collect().await;
    assert!(!collected.is_empty(), "the clean turn relays events");
    let (count, latencies) = crate::metrics::proxied_request_samples_for_tests(
        "early-metrics-probe",
        "gpt-5.2-codex",
        200,
    );
    assert_eq!(count, 1, "exactly one sample for the classified attempt");
    assert!(
        latencies.iter().all(|latency| *latency >= 50.0),
        "the latency covers the real upstream round-trip, not the near-zero commit, got {latencies:?}"
    );
}

/// A classified failure records its real status, not the committed 200.
#[tokio::test]
async fn http_events_stream_records_the_terminal_status_of_a_classified_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_string("{}"))
        .mount(&server)
        .await;
    let state = state_with_provider("early-metrics-fail-probe", server.uri());
    let events = http_events_stream(
        HttpSendContext {
            state,
            route: named_codex_route("early-metrics-fail-probe"),
            policy: crate::retry::RetryPolicy::DISABLED,
            credential: None,
            session_id: None,
            upstream_body: std::sync::Arc::new(json!({"input": []})),
            auth: crate::config::AuthMode::ApiKey,
            codex_quota_account: None,
        },
        CredentialSource::Resolved(Credential::ApiKey {
            value: "probe".to_string(),
            header: crate::config::ApiKeyHeader::Bearer,
        }),
        None,
        None,
    );
    use futures_util::StreamExt;
    let collected: Vec<_> = events.collect().await;
    assert_eq!(collected.len(), 1, "one terminal item");
    let (count, _) = crate::metrics::proxied_request_samples_for_tests(
        "early-metrics-fail-probe",
        "gpt-5.2-codex",
        429,
    );
    assert_eq!(count, 1, "the classified failure records its real status");
}

/// A credential-resolution failure classifies with its own status: the
/// sample records it, never the committed 200.
#[tokio::test]
async fn http_events_stream_records_the_credential_resolution_failure_status() {
    let state = state_with_provider("early-metrics-cred-probe", "http://127.0.0.1:1".to_string());
    let events = http_events_stream(
        HttpSendContext {
            state,
            route: named_codex_route("early-metrics-cred-probe"),
            policy: crate::retry::RetryPolicy::DISABLED,
            credential: None,
            session_id: None,
            upstream_body: std::sync::Arc::new(json!({"input": []})),
            auth: crate::config::AuthMode::ApiKey,
            codex_quota_account: None,
        },
        CredentialSource::Deferred(Box::pin(async {
            Err(crate::adapters::AdapterError {
                message: "no key".to_string(),
                response: Box::new(
                    crate::error::ShuntError::new(
                        StatusCode::UNAUTHORIZED,
                        "authentication_error",
                        "missing api key".to_string(),
                    )
                    .into_response(),
                ),
                failure: None,
            })
        })),
        None,
        None,
    );
    use futures_util::StreamExt;
    let collected: Vec<_> = events.collect().await;
    assert_eq!(collected.len(), 1, "one terminal item");
    let (count, _) = crate::metrics::proxied_request_samples_for_tests(
        "early-metrics-cred-probe",
        "gpt-5.2-codex",
        401,
    );
    assert_eq!(count, 1, "the credential failure records its own status");
}

/// Only the committed streaming responses carry the in-stream-metrics
/// marker the failover loop checks; the already-upstreamed `stream_response`
/// must keep its loop sample.
#[tokio::test]
async fn only_the_committed_streaming_responses_carry_the_metrics_marker() {
    let server = MockServer::start().await;
    let upstream = reqwest::get(server.uri()).await.expect("mock responds");
    let machine = relay_opts()
        .machine()
        .with_input_estimate(0)
        .without_content_accumulation();
    let committed = early_streaming_response(
        move || {
            Box::pin(async move {
                let mut machine = machine;
                let start = machine.start_synthetic(format!("msg_{}", uuid::Uuid::new_v4()));
                (machine, start.join(""))
            })
        },
        std::time::Duration::from_secs(30),
        futures_util::stream::pending::<Result<ResponseEvent, Value>>(),
    );
    assert!(committed
        .extensions()
        .get::<crate::adapters::responses::InStreamMetrics>()
        .is_some());
    let pooled = pool_streaming_response(
        move || {
            Box::pin(async move {
                let mut machine = relay_opts()
                    .machine()
                    .with_input_estimate(0)
                    .without_content_accumulation();
                let start = machine.start_synthetic(format!("msg_{}", uuid::Uuid::new_v4()));
                (machine, start.join(""))
            })
        },
        std::time::Duration::from_secs(30),
        futures_util::stream::pending::<Result<PoolItem, Value>>(),
    );
    assert!(pooled
        .extensions()
        .get::<crate::adapters::responses::InStreamMetrics>()
        .is_some());
    let relayed = super::super::http::stream_response(
        upstream,
        relay_opts(),
        0,
        std::time::Duration::from_secs(30),
    );
    assert!(relayed
        .extensions()
        .get::<crate::adapters::responses::InStreamMetrics>()
        .is_none());
}

/// A terminal non-advance status defers its error-body read: the classified
/// failure returns at header arrival with the envelope unbuilt, so the caller
/// records the latency sample before the (budgeted) body read — never after
/// it, the way an eager read would inflate the sample.
#[tokio::test]
async fn a_terminal_status_defers_its_error_body_read() {
    let stalled = crate::testutil::StalledBody::start(StatusCode::BAD_REQUEST, "{", "}").await;
    let state = state_with_provider("stalled-400-send-probe", stalled.base_url.clone());
    let context = HttpSendContext {
        state,
        route: named_codex_route("stalled-400-send-probe"),
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
    let classified = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        send_classified(&context),
    )
    .await
    .expect("the classified failure returns at header arrival, before the stalled body resolves");
    let envelope = match classified {
        SendClassified::Failed {
            envelope, status, ..
        } => {
            assert_eq!(status, StatusCode::BAD_REQUEST);
            envelope
        }
        SendClassified::Relay { .. } => panic!("a 400 classifies as a failure"),
    };
    // The body read happens at resolve: release the stalled body and build
    // the envelope, which still names the status.
    let (resolved, _) = tokio::join!(async { envelope.resolve().await }, stalled.release());
    assert!(
        resolved.get("error").is_some(),
        "the envelope maps the 400, got {resolved:?}"
    );
}

/// The latency sample for a terminal status records at header arrival: while
/// the error body is still stalled, the sample is already in the registry
/// (and small), and only the terminal error frame waits for the body.
#[tokio::test]
async fn a_stalled_terminal_error_body_never_delays_the_latency_sample() {
    let stalled = crate::testutil::StalledBody::start(StatusCode::BAD_REQUEST, "{", "}").await;
    let state = state_with_provider("stalled-400-metrics-probe", stalled.base_url.clone());
    let mut events = Box::pin(http_events_stream(
        HttpSendContext {
            state,
            route: named_codex_route("stalled-400-metrics-probe"),
            policy: crate::retry::RetryPolicy::DISABLED,
            credential: None,
            session_id: None,
            upstream_body: std::sync::Arc::new(json!({"input": []})),
            auth: crate::config::AuthMode::ApiKey,
            codex_quota_account: None,
        },
        CredentialSource::Resolved(Credential::ApiKey {
            value: "probe".to_string(),
            header: crate::config::ApiKeyHeader::Bearer,
        }),
        None,
        None,
    ));
    use futures_util::StreamExt;
    let drive = tokio::spawn(async move { events.next().await });
    let count = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let (count, _) = crate::metrics::proxied_request_samples_for_tests(
                "stalled-400-metrics-probe",
                "gpt-5.2-codex",
                400,
            );
            if count > 0 {
                break count;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the sample records at header arrival, before the stalled body resolves");
    assert_eq!(count, 1, "exactly one sample for the classified attempt");
    let (_, latencies) = crate::metrics::proxied_request_samples_for_tests(
        "stalled-400-metrics-probe",
        "gpt-5.2-codex",
        400,
    );
    assert!(
        latencies.iter().all(|latency| *latency < 1000.0),
        "the latency covers headers only, never the stalled body read, got {latencies:?}"
    );
    stalled.release().await;
    let item = drive.await.unwrap();
    assert!(
        item.is_some_and(|item| item.is_err()),
        "the turn ends in one terminal error item"
    );
}

/// A still-pending estimate defers only the relay build: the helper returns
/// a pending relay without awaiting the estimate — the chain records the
/// winner before that await, so a serialized build never returns and the
/// timeout fails the test.
#[tokio::test]
async fn a_pending_estimate_defers_only_the_relay_build() {
    use crate::proxy::chain_stream::RelayBuild;
    use futures_util::TryStreamExt;
    let (estimate_release, estimate_wait) = tokio::sync::oneshot::channel::<()>();
    let estimate_resolved = std::sync::Arc::new(std::sync::Mutex::new(false));
    let estimate_resolved_flag = estimate_resolved.clone();
    let estimate = EstimateBuild::Pending(Box::pin(async move {
        let _ = estimate_wait.await;
        *estimate_resolved_flag.lock().expect("estimate flag lock") = true;
        42u64
    }));
    let built_value = std::sync::Arc::new(std::sync::Mutex::new(None::<u64>));
    let built_value_flag = built_value.clone();
    let relay = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        std::future::ready(relay_build(
            estimate,
            move |value| {
                *built_value_flag.lock().expect("built value lock") = Some(value);
                let mut machine = relay_opts().machine().without_content_accumulation();
                let start = machine.start_synthetic(format!("msg_{}", uuid::Uuid::new_v4()));
                (machine, start)
            },
            |machine| {
                Box::pin(
                    translated_stream(futures_util::stream::empty(), machine)
                        .map_err(|never| match never {}),
                )
            },
        )),
    )
    .await
    .expect("the relay build never waits on the pending estimate");
    let RelayBuild::Pending(build) = relay else {
        panic!("the estimate was still pending when the relay built");
    };
    assert!(
        !*estimate_resolved.lock().expect("estimate flag lock"),
        "the estimate stays unawaited until the chain awaits the pending relay"
    );
    estimate_release
        .send(())
        .expect("the estimate is still listening");
    let (start, _frames) = build.await;
    assert!(
        String::from_utf8_lossy(&start).contains("event: message_start"),
        "the pending relay resolves the synthetic start, got: {}",
        String::from_utf8_lossy(&start)
    );
    assert_eq!(
        *built_value.lock().expect("built value lock"),
        Some(42),
        "the build receives the resolved estimate"
    );
}

/// A resolved estimate builds the relay now: the winner relays the synthetic
/// start immediately, with no pending future.
#[tokio::test]
async fn a_resolved_estimate_builds_the_relay_now() {
    use crate::proxy::chain_stream::RelayBuild;
    use futures_util::TryStreamExt;
    let built_value = std::sync::Arc::new(std::sync::Mutex::new(None::<u64>));
    let built_value_flag = built_value.clone();
    let relay = relay_build(
        EstimateBuild::Ready(42),
        move |value| {
            *built_value_flag.lock().expect("built value lock") = Some(value);
            let mut machine = relay_opts().machine().without_content_accumulation();
            let start = machine.start_synthetic(format!("msg_{}", uuid::Uuid::new_v4()));
            (machine, start)
        },
        |machine| {
            Box::pin(
                translated_stream(futures_util::stream::empty(), machine)
                    .map_err(|never| match never {}),
            )
        },
    );
    let RelayBuild::Ready { start, .. } = relay else {
        panic!("the resolved estimate builds the relay immediately");
    };
    let start = start.expect("a Responses-kind winner always carries the synthetic start");
    assert!(
        String::from_utf8_lossy(&start).contains("event: message_start"),
        "got: {}",
        String::from_utf8_lossy(&start)
    );
    assert_eq!(
        *built_value.lock().expect("built value lock"),
        Some(42),
        "the build receives the resolved estimate"
    );
}

/// A still-pending machine build defers only the pooled relay: the helper
/// returns a pending relay without awaiting the build, and the resolved
/// relay puts the buffered account attribution frame first.
#[tokio::test]
async fn a_pending_machine_build_defers_only_the_pool_relay() {
    use crate::adapters::responses::sse_parse::{pool_relay_build, MachineBuild, PoolEvent};
    use crate::proxy::chain_stream::RelayBuild;
    let (build_release, build_wait) = tokio::sync::oneshot::channel::<()>();
    let build_resolved = std::sync::Arc::new(std::sync::Mutex::new(false));
    let build_resolved_flag = build_resolved.clone();
    let machine = relay_opts().machine().without_content_accumulation();
    let build = async move {
        let _ = build_wait.await;
        *build_resolved_flag.lock().expect("build flag lock") = true;
        let mut machine = machine;
        let start = machine.start_synthetic(format!("msg_{}", uuid::Uuid::new_v4()));
        (machine, start)
    };
    let relay = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        std::future::ready(pool_relay_build(
            MachineBuild::Pending(Box::pin(build)),
            PoolEvent::Account("pool-a".to_string()),
            Box::pin(futures_util::stream::empty()),
        )),
    )
    .await
    .expect("the pooled relay build never waits on the pending machine build");
    let RelayBuild::Pending(build) = relay else {
        panic!("the machine build was still pending when the relay built");
    };
    assert!(
        !*build_resolved.lock().expect("build flag lock"),
        "the machine build stays unawaited until the chain awaits the pending relay"
    );
    build_release
        .send(())
        .expect("the build is still listening");
    let (start, mut frames) = build.await;
    assert!(
        String::from_utf8_lossy(&start).contains("event: message_start"),
        "the pending relay resolves the synthetic start"
    );
    use futures_util::StreamExt;
    // The frames stream's leading chunk is the (empty) factory leading
    // bytes: the synthetic start already went out as the winner's `start`.
    let first = loop {
        let chunk = frames
            .next()
            .await
            .expect("the relayed frames yield")
            .expect("the frame is ok");
        if !chunk.is_empty() {
            break chunk;
        }
    };
    assert_eq!(
        String::from_utf8_lossy(&first),
        "event: account\ndata: \"pool-a\"\n\n",
        "the buffered account attribution frame stays first"
    );
}

/// A resolved machine build relays the buffered account attribution frame
/// first, exactly like the pending arm.
#[tokio::test]
async fn a_resolved_machine_build_relays_the_account_frame_first() {
    use crate::adapters::responses::sse_parse::{pool_relay_build, MachineBuild, PoolEvent};
    use crate::proxy::chain_stream::RelayBuild;
    let mut machine = relay_opts().machine().without_content_accumulation();
    let start = machine.start_synthetic(format!("msg_{}", uuid::Uuid::new_v4()));
    let relay = pool_relay_build(
        MachineBuild::Ready(Box::new(machine), start),
        PoolEvent::Account("pool-a".to_string()),
        Box::pin(futures_util::stream::empty()),
    );
    let RelayBuild::Ready {
        start, mut frames, ..
    } = relay
    else {
        panic!("the resolved build relays immediately");
    };
    let start = start.expect("a Responses-kind winner always carries the synthetic start");
    assert!(
        String::from_utf8_lossy(&start).contains("event: message_start"),
        "got: {}",
        String::from_utf8_lossy(&start)
    );
    use futures_util::StreamExt;
    // The frames stream's leading chunk is the (empty) factory leading
    // bytes: the synthetic start already went out as the winner's `start`.
    let first = loop {
        let chunk = frames
            .next()
            .await
            .expect("the relayed frames yield")
            .expect("the frame is ok");
        if !chunk.is_empty() {
            break chunk;
        }
    };
    assert_eq!(
        String::from_utf8_lossy(&first),
        "event: account\ndata: \"pool-a\"\n\n",
        "the account attribution frame relays first"
    );
}

/// The non-pooled winner arm returns the relay without awaiting a
/// still-pending estimate: pre-seeding the chain's shared estimate with a
/// pending share pins the send-win race, and a serialized arm awaits the
/// pending estimate forever — the timeout fails the test.
#[tokio::test]
async fn a_winner_with_a_pending_estimate_returns_a_pending_relay_build() {
    use crate::adapters::responses::{chain_attempt, ChainEstimate};
    use crate::auth::shared::EnvVarGuard;
    use crate::proxy::chain_stream::{Attempt, RelayBuild};
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(""))
        .mount(&server)
        .await;
    // `api_key` auth: the built-in codex provider defaults to
    // `chatgpt_oauth`, whose single-credential resolution reads the real
    // account store — box-dependent.
    let key = "SHUNT_TEST_CHAIN_ATTEMPT_KEY";
    let _env = EnvVarGuard::set(key, "probe");
    let mut config = crate::config::Config::default();
    let mut provider = config
        .providers
        .get("codex")
        .expect("codex provider is built in")
        .clone();
    provider.base_url = server.uri();
    provider.auth = crate::config::AuthMode::ApiKey;
    provider.api_key_env = Some(key.to_string());
    config
        .providers
        .insert("pending-estimate-probe".to_string(), provider);
    let state = AppState::new(config, reqwest::Client::new()).unwrap();
    let route = named_codex_route("pending-estimate-probe");
    let body = crate::request::RequestBody::parse(
        serde_json::json!({
            "model": "gpt-5.6-sol",
            "stream": true,
            "messages": [{ "role": "user", "content": "hi" }],
        })
        .to_string()
        .into_bytes(),
    )
    .expect("the request body parses");
    let cache = ChainEstimate::test_held_pending();
    let attempt = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        chain_attempt(&state, &route, &axum::http::HeaderMap::new(), body, &cache),
    )
    .await
    .expect("the winner arm returns without awaiting the pending estimate");
    let Attempt::Winner { relay, .. } = attempt else {
        panic!("the 200 upstream wins the attempt");
    };
    assert!(
        matches!(relay, RelayBuild::Pending(_)),
        "the estimate is still pending when the winner returns"
    );
}

/// The pooled winner arm passes the pending machine build through without
/// awaiting it, exactly like the non-pooled arm: the shared estimate cell
/// stays pending, the pool's first item (the account attribution frame)
/// wins the race, and the arm returns a pending relay — a serialized arm
/// awaits the pending build forever and the timeout fails the test.
#[tokio::test]
async fn a_pooled_winner_with_a_pending_estimate_returns_a_pending_relay_build() {
    use crate::adapters::responses::{chain_attempt, ChainEstimate};
    use crate::proxy::chain_stream::{Attempt, RelayBuild};
    use base64::Engine;
    let dir = std::env::temp_dir().join(format!(
        "shunt-pooled-chain-attempt-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let payload = serde_json::json!({
        "exp": 4_102_444_800u64,
        "https://api.openai.com/auth": {"chatgpt_account_id": "acc-pooled-chain"}
    });
    let access_token = format!(
        "x.{}.y",
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap())
    );
    let credentials = dir.join("auth.json");
    std::fs::write(
        &credentials,
        serde_json::json!({
            "auth_mode": "ChatGPT",
            "tokens": {
                "access_token": access_token,
                "refresh_token": "unused-refresh-token"
            }
        })
        .to_string(),
    )
    .unwrap();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "event: response.created\ndata: {\"response\":{\"id\":\"resp_1\"}}\n\n",
                ),
        )
        .mount(&server)
        .await;
    let mut config = crate::config::Config::default();
    let mut provider = config
        .providers
        .get("codex")
        .expect("codex provider is built in")
        .clone();
    provider.base_url = server.uri();
    provider.accounts = vec![crate::config::AccountConfig {
        name: "pool-a".to_string(),
        credentials: Some(credentials.to_string_lossy().into_owned()),
        ..Default::default()
    }];
    config
        .providers
        .insert("pooled-chain-probe".to_string(), provider);
    let state = AppState::new(config, reqwest::Client::new()).unwrap();
    let route = named_codex_route("pooled-chain-probe");
    let body = crate::request::RequestBody::parse(
        serde_json::json!({
            "model": "gpt-5.6-sol",
            "stream": true,
            "messages": [{ "role": "user", "content": "hi" }],
        })
        .to_string()
        .into_bytes(),
    )
    .expect("the request body parses");
    let cache = ChainEstimate::test_held_pending();
    let attempt = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        chain_attempt(&state, &route, &axum::http::HeaderMap::new(), body, &cache),
    )
    .await
    .expect("the pooled winner arm returns without awaiting the pending build");
    let Attempt::Winner { relay, .. } = attempt else {
        panic!("the pooled account wins the attempt");
    };
    assert!(
        matches!(relay, RelayBuild::Pending(_)),
        "the estimate is still pending when the pooled winner returns"
    );
}
