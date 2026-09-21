//! Bound tests for the internal-call collectors (ADR-0005 §3, issue #594).
//!
//! Each test names the bound it is about, because the point of a closed
//! [`BoundExceeded`] set is that an operator can read the failure back to the
//! key they wrote. Non-vacuity: make [`collect_bounded`] buffer first and check
//! afterwards and `an_oversized_body_is_refused` still passes — which is why it
//! asserts the *cap* it reports rather than only that it failed; delete the
//! ping check in `is_ping_frame` and
//! `a_ping_only_chunk_does_not_reset_the_idle_gap` goes red, because the
//! keep-alive stream then runs forever; drop the carried remainder in
//! `take_complete_frames` and
//! `a_ping_frame_split_across_chunks_does_not_reset_the_idle_gap` goes red,
//! because each half is then classified on its own; drop the `hard_deadline`
//! comparison and `a_stream_past_its_wall_clock_bound_reports_duration` reports
//! `Idle`.

use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;

use super::bounds::{bound_stream, collect_bounded, BoundExceeded, CollectError, GatedBounds};

fn gated(max_bytes: usize, idle_ms: u64, duration_ms: u64) -> GatedBounds {
    GatedBounds {
        max_bytes,
        idle: Duration::from_millis(idle_ms),
        max_duration: Duration::from_millis(duration_ms),
    }
}

/// One chunk every `gap`, forever. `ping` decides whether each chunk is an SSE
/// keep-alive frame or real content.
fn heartbeat(gap: Duration, ping: bool) -> impl futures_util::Stream<Item = Bytes> {
    let frame: &'static [u8] = if ping {
        b"event: ping\ndata: {\"type\":\"ping\"}\n\n"
    } else {
        b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n"
    };
    futures_util::stream::unfold((), move |()| async move {
        tokio::time::sleep(gap).await;
        Some((Bytes::from_static(frame), ()))
    })
}

#[tokio::test]
async fn a_body_within_its_cap_is_collected_whole() {
    let body = axum::body::Body::from("0123456789");
    let collected = collect_bounded(body, 10)
        .await
        .expect("10 bytes fits in 10");
    assert_eq!(collected, Bytes::from_static(b"0123456789"));
}

#[tokio::test]
async fn an_oversized_body_is_refused() {
    let body = axum::body::Body::from("0123456789");
    let error = collect_bounded(body, 9)
        .await
        .expect_err("10 bytes does not fit in 9");
    let CollectError::Oversized(oversized) = error else {
        panic!("a body over the cap is refused as oversized, got {error:?}");
    };
    assert_eq!(
        oversized.max_bytes, 9,
        "the refusal names the cap that was crossed, not the body's size"
    );
}

/// A body stream that breaks mid-reply is a failed transport, not a reply.
///
/// Returning the bytes read so far reports success for a truncated body, and
/// the caller's JSON parse then records the call as `invalid_reply` — the
/// judge answered with something malformed — for what is actually a network
/// fault. Non-vacuity: return `Ok` on the stream error and the `expect_err`
/// below goes red with `partial` in hand.
#[tokio::test]
async fn a_broken_body_stream_is_a_transport_failure_not_a_short_body() {
    let stream = futures_util::stream::iter(vec![
        Ok(Bytes::from_static(b"partial")),
        Err(std::io::Error::other("connection reset by peer")),
    ]);
    let body = axum::body::Body::from_stream(stream);

    let error = collect_bounded(body, 1024)
        .await
        .expect_err("a body that never finished arriving is not a whole body");

    assert!(
        matches!(error, CollectError::Transport(_)),
        "a stream failure is its own fault, not the byte cap; got {error:?}"
    );
}

#[tokio::test]
async fn a_stream_inside_every_bound_passes_through_unchanged() {
    let source = futures_util::stream::iter(vec![
        Bytes::from_static(b"first"),
        Bytes::from_static(b"second"),
    ]);
    let collected: Vec<_> = bound_stream(source, gated(1024, 60_000, 600_000))
        .collect()
        .await;
    assert_eq!(
        collected,
        vec![
            Ok(Bytes::from_static(b"first")),
            Ok(Bytes::from_static(b"second")),
        ],
        "a well-behaved stream is relayed byte for byte"
    );
}

#[tokio::test]
async fn a_stream_past_its_byte_cap_reports_max_bytes() {
    let source = futures_util::stream::iter(vec![
        Bytes::from_static(b"1234"),
        Bytes::from_static(b"5678"),
    ]);
    let collected: Vec<_> = bound_stream(source, gated(5, 60_000, 600_000))
        .collect()
        .await;
    assert_eq!(
        collected,
        vec![
            Ok(Bytes::from_static(b"1234")),
            Err(BoundExceeded::MaxBytes),
        ],
        "the chunk that crosses the cap ends the stream"
    );
}

/// The `200`-then-stall shape: headers committed, then nothing.
#[tokio::test(start_paused = true)]
async fn a_silent_stream_reports_idle() {
    let source = futures_util::stream::pending::<Bytes>();
    let collected: Vec<_> = bound_stream(source, gated(1024, 50, 600_000))
        .collect()
        .await;
    assert_eq!(collected, vec![Err(BoundExceeded::Idle)]);
}

/// The endless-ping shape. The socket is alive and chunks keep arriving, so a
/// timer reset by *any* chunk would never fire — which is the whole reason
/// `is_ping_only` exists.
#[tokio::test(start_paused = true)]
async fn a_ping_only_chunk_does_not_reset_the_idle_gap() {
    let collected: Vec<_> = bound_stream(
        heartbeat(Duration::from_millis(10), true),
        gated(1024 * 1024, 50, 600_000),
    )
    .collect()
    .await;
    assert_eq!(
        collected.last(),
        Some(&Err(BoundExceeded::Idle)),
        "an endless keep-alive stream still runs out of idle budget"
    );
    // The control: the same cadence carrying content never reaches the gap.
    let content: Vec<_> = bound_stream(
        heartbeat(Duration::from_millis(10), false),
        gated(1024 * 1024, 50, 200),
    )
    .collect()
    .await;
    assert_eq!(
        content.last(),
        Some(&Err(BoundExceeded::Duration)),
        "a progressing stream is ended by the wall clock, not by the idle gap"
    );
}

/// The endless-ping shape again, with each frame split *inside a line* so no
/// chunk is a ping frame on its own.
///
/// A classifier that looks at one chunk at a time says "not a ping" to both
/// halves — `event: pin` has no terminator and `g\n\n` has no `event:` line —
/// and refreshes the idle deadline twice for zero delivered content, which
/// leaves `gated_idle_ms` unreachable and the stream running to
/// `gated_max_duration_ms`. The split is deliberately mid-line rather than at a
/// frame boundary, so a fix that merely buffered whole *lines* could not pass
/// this either.
#[tokio::test(start_paused = true)]
async fn a_ping_frame_split_across_chunks_does_not_reset_the_idle_gap() {
    let halves: &[&'static [u8]] = &[b"event: pin", b"g\ndata: {\"type\":\"ping\"}\n\n"];
    let stream = futures_util::stream::unfold(0usize, move |index| async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        Some((Bytes::from_static(halves[index % 2]), index + 1))
    });

    let collected: Vec<_> = bound_stream(stream, gated(1024 * 1024, 50, 600_000))
        .collect()
        .await;

    assert_eq!(
        collected.last(),
        Some(&Err(BoundExceeded::Idle)),
        "a keep-alive stream is a keep-alive however the upstream splits it, \
         so the idle gap still runs out"
    );
}

/// The control for the test above: the same mid-line splitting, carrying
/// content. Reassembly must not turn every split frame into a stall — a
/// content frame that completes is progress no matter which chunk finished it,
/// so this stream reaches the wall clock rather than the idle gap.
#[tokio::test(start_paused = true)]
async fn a_content_frame_split_across_chunks_still_counts_as_progress() {
    let halves: &[&'static [u8]] = &[
        b"event: content_bl",
        b"ock_delta\ndata: {\"type\":\"content_block_delta\"}\n\n",
    ];
    let stream = futures_util::stream::unfold(0usize, move |index| async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        Some((Bytes::from_static(halves[index % 2]), index + 1))
    });

    let collected: Vec<_> = bound_stream(stream, gated(1024 * 1024, 50, 200))
        .collect()
        .await;

    assert_eq!(
        collected.last(),
        Some(&Err(BoundExceeded::Duration)),
        "a split content frame is still content once it completes"
    );
}

/// A malformed byte must not stall framing for the rest of the stream.
///
/// A frame carrying one is still a delivered frame: the bytes around it are
/// a `content_block_delta` whatever the payload decodes to. Framing it would
/// hold the buffer for a sequence no later chunk can complete — each chunk
/// re-scans it, fails identically, and yields no frame — so the idle deadline
/// never refreshes and a stream delivering content the whole time reports
/// `Idle`. Non-vacuity: gate framing on a successful decode of the buffer
/// (`std::str::from_utf8(scan)`, returning no frame on `Err`) and this goes
/// red with `Idle` at 50ms.
#[tokio::test(start_paused = true)]
async fn a_malformed_sequence_does_not_stall_framing() {
    // `0xff` is valid UTF-8 in no position — the case a decode-first framer
    // cannot tell apart from a character whose rest is still arriving.
    let chunk: &'static [u8] = b"event: content_block_delta\ndata: {\"t\":\"\xff\"}\n\n";
    let stream = futures_util::stream::unfold(0usize, move |index| async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        Some((Bytes::from_static(chunk), index + 1))
    });

    let collected: Vec<_> = bound_stream(stream, gated(1024 * 1024, 50, 200))
        .collect()
        .await;

    assert_eq!(
        collected.last(),
        Some(&Err(BoundExceeded::Duration)),
        "a frame carrying an undecodable byte is still a delivered frame"
    );
}

/// A buffer ending mid-character must still deliver the frames that finished
/// before it.
///
/// Where a chunk ends is the network's choice, so a remainder that stops
/// inside a multi-byte character is the ordinary case. Framing the buffer by
/// decoding it first has to answer for that partial character, and both
/// answers are wrong: refusing to frame until it completes strands every
/// finished frame behind it, and decoding lossily writes `U+FFFD` over it and
/// carries *that* forward, so the continuation bytes in the next chunk land
/// after a character nothing can complete. Scanning bytes asks neither
/// question — `\n` is ASCII and no byte of a multi-byte character is.
///
/// The stream here is built so the buffer never once ends on a whole
/// character, which is what makes the stall total rather than intermittent.
/// Non-vacuity: gate framing on `std::str::from_utf8(scan)` succeeding and
/// this goes red with `Idle` at 50ms.
#[tokio::test(start_paused = true)]
async fn a_chunk_ending_mid_character_still_delivers_its_finished_frames() {
    // Each chunk closes the previous chunk's dangling `é` and opens a new one,
    // so every buffer state holds a complete frame *and* a trailing partial
    // character. `0xc3` is the lead byte of a two-byte character; `0xa9` is
    // its continuation.
    let head: &'static [u8] = b"event: content_block_delta\ndata: {}\n\n\xc3";
    let tail: &'static [u8] = b"\xa9\n\nevent: content_block_delta\ndata: {}\n\n\xc3";
    let stream = futures_util::stream::unfold(0usize, move |index| async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let chunk = if index == 0 { head } else { tail };
        Some((Bytes::from_static(chunk), index + 1))
    });

    let collected: Vec<_> = bound_stream(stream, gated(1024 * 1024, 50, 200))
        .collect()
        .await;

    assert_eq!(
        collected.last(),
        Some(&Err(BoundExceeded::Duration)),
        "the finished frames are delivered, so the idle bound keeps refreshing"
    );
}

/// The same guard under CRLF framing. SSE terminates a line with CRLF as
/// legitimately as with LF, and `\r\n\r\n` holds no literal `\n\n` — so a chunk
/// split on `\n\n` alone never divides there and collapses into a single frame.
/// That frame's `event: ping` line then makes the *whole* chunk read as a
/// keep-alive even though a `content_block_delta` rides in it, leaving the idle
/// bound armed against a stream that was delivering content the entire time.
/// Drop the normalization and this reports `Idle` at 50ms instead.
#[tokio::test(start_paused = true)]
async fn a_crlf_chunk_carrying_content_is_not_ping_only() {
    // One ping frame and one real frame in the same chunk, CRLF throughout.
    let frame: &'static [u8] = b"event: ping\r\ndata: {\"type\":\"ping\"}\r\n\r\nevent: content_block_delta\r\ndata: {\"type\":\"content_block_delta\"}\r\n\r\n";
    let stream = futures_util::stream::unfold((), move |()| async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        Some((Bytes::from_static(frame), ()))
    });

    let collected: Vec<_> = bound_stream(stream, gated(1024 * 1024, 50, 200))
        .collect()
        .await;

    assert_eq!(
        collected.last(),
        Some(&Err(BoundExceeded::Duration)),
        "a CRLF chunk carrying a content frame is progress, so the wall clock \
         ends the stream rather than the idle gap"
    );
}

#[tokio::test(start_paused = true)]
async fn a_stream_past_its_wall_clock_bound_reports_duration() {
    let collected: Vec<_> = bound_stream(
        heartbeat(Duration::from_millis(10), false),
        gated(1024 * 1024, 60_000, 100),
    )
    .collect()
    .await;
    assert_eq!(collected.last(), Some(&Err(BoundExceeded::Duration)));
}

/// The deployment the judge-call tests below run against: one provider that is
/// the judge, three aliases mapped on it, and a driven entry pointed at them.
///
/// Shared so each test differs only in what its judge upstream does.
mod judge_fixture {
    use std::collections::BTreeMap;

    use crate::config::{
        AuthMode, Config, ModelConfig, ProviderConfig, RouterConfig, StageClassifierConfig,
        StageRouterConfig, StageRouterPicker,
    };
    use crate::server::AppState;

    fn provider(base_url: String) -> ProviderConfig {
        let mut provider = Config::default()
            .providers
            .remove("anthropic")
            .expect("the default config ships an anthropic provider");
        provider.base_url = base_url;
        // `None`, so the route is credential-injecting (and so a legal judge
        // target) without this test needing an environment variable.
        provider.auth = AuthMode::None;
        provider
    }

    fn mapped(id: &str, upstream_model: &str) -> ModelConfig {
        ModelConfig {
            subagents: None,
            id: id.to_string(),
            display_name: None,
            upstream_model: Some(BTreeMap::from([(
                "judge".to_string(),
                upstream_model.to_string(),
            )])),
            router: None,
            stage_router: None,
        }
    }

    /// The driven entry, with a deadline short enough that a test which reaches
    /// it finishes promptly — and long enough that reaching it is a finding
    /// rather than a flake.
    pub(super) fn stage() -> StageRouterConfig {
        StageRouterConfig {
            classifier: Some(StageClassifierConfig {
                target: "judge-alias".to_string(),
                base_threshold: 0.5,
            }),
            judge_timeout_ms: 2_000,
            ..StageRouterConfig::preset(
                "capable-alias".to_string(),
                "efficient-alias".to_string(),
                StageRouterPicker::EfficientFirst,
                0.5,
            )
        }
    }

    pub(super) fn state(router_id: &str, judge_url: String, stage: &StageRouterConfig) -> AppState {
        let mut config = Config {
            models: vec![
                ModelConfig {
                    subagents: None,
                    id: router_id.to_string(),
                    display_name: None,
                    upstream_model: None,
                    router: Some(RouterConfig::StageRouter(stage.clone())),
                    stage_router: None,
                },
                mapped("capable-alias", "upstream-capable"),
                mapped("efficient-alias", "upstream-efficient"),
                mapped("judge-alias", "upstream-judge"),
            ],
            ..Config::default()
        };
        config.providers = BTreeMap::from([("judge".to_string(), provider(judge_url))]);
        config.server.default_provider = "judge".to_string();
        AppState::new(config, reqwest::Client::new()).expect("the config is valid")
    }
}

/// The judge's upstream call is an ordinary proxied request, separated from the
/// client turn it was made for by one attribute.
///
/// In-crate rather than in `tests/router_judge.rs` because the sample store is
/// `cfg(test)` and an integration binary links the library without it. Drop the
/// `caller` argument at `run_chain`'s call site in [`super::dispatch`] and this
/// goes red on a sample filed under `client`.
mod caller_attribution {
    use axum::http::HeaderMap;
    use serde_json::json;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use crate::proxy::failover::InboundContext;
    use crate::routing::judge::{consult, JudgeOutcome};
    use crate::routing::serve::AdmittedContext;
    use crate::routing::stage::StageTier;

    use super::judge_fixture;

    const ROUTER_ID: &str = "claude-auto-caller-metric";

    #[tokio::test]
    async fn a_judge_call_is_recorded_under_the_router_caller() {
        let judge = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    json!({
                        "id": "msg_judge",
                        "type": "message",
                        "role": "assistant",
                        "model": "upstream-judge",
                        "content": [{"type": "text", "text": json!({
                            "crux": "bounded task",
                            "primary_rule": "SUP-1",
                            "capability_boundary": "supported",
                            "p_solve": 0.1,
                        }).to_string()}],
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 1, "output_tokens": 1},
                    })
                    .to_string(),
                ),
            )
            .mount(&judge)
            .await;

        let stage = judge_fixture::stage();
        let state = judge_fixture::state(ROUTER_ID, judge.uri(), &stage);

        let inbound = InboundContext::internal();
        let headers = HeaderMap::new();
        let admitted = AdmittedContext::mint(&inbound, &headers, ROUTER_ID);
        let request = json!({
            "model": ROUTER_ID,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        });
        let classifier = stage
            .classifier
            .as_ref()
            .expect("the fixture names a judge");

        let outcome = consult(&state, &admitted, &stage, classifier, &request).await;

        assert_eq!(
            outcome,
            JudgeOutcome::Decided(StageTier::Capable),
            "a `p_solve` under the threshold is the judge declining the efficient tier"
        );
        let (router, _) = crate::metrics::proxied_request_samples_by_caller_for_tests(
            "router", "judge", ROUTER_ID, 200,
        );
        let (client, _) = crate::metrics::proxied_request_samples_by_caller_for_tests(
            "client", "judge", ROUTER_ID, 200,
        );
        assert_eq!(router, 1, "the judge's own call is the router's");
        assert_eq!(client, 0, "and is not attributed to the caller's turn");
    }
}

/// `judge_max_response_bytes` has to bound what the call *allocates*, not just
/// what reaches the JSON parser.
///
/// The gap this closes: `routing::resolve_target_chain` stamps the advertised
/// router id onto `Route.model` while `Route.upstream_model` stays the judge's
/// own, so every realistic judge reply takes the Anthropic adapter's alias
/// branch — the one that reads the whole upstream body to rewrite its top-level
/// `model`. That read used to be `reqwest`'s unbounded `bytes()`, and
/// [`super::bounds::collect_bounded`] ran on its output, so the cap bounded the
/// parser's input and nothing else.
///
/// The oracle is the outcome *label*, because a finite oversized body is
/// refused by the collector too and so cannot tell the two caps apart. This
/// judge answers `200` and then streams for as long as anyone reads. Capped at
/// the read, the call reports `oversized` in milliseconds, having taken a
/// little over the cap. Uncapped, the adapter drains whatever the upstream
/// sends and the call ends on whatever that drain produces — the deadline, or
/// the abandoned connection — which is some other operator's key for an
/// upstream that is not slow and did not fail.
///
/// In-crate rather than in `tests/router_judge.rs` for the same reason
/// [`caller_attribution`] is: it asserts on shunt's own view of the call rather
/// than on what a client sees, and a fail-open looks identical from outside.
mod oversized_reply {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::{Duration, Instant};

    use axum::http::HeaderMap;
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use crate::proxy::failover::InboundContext;
    use crate::routing::judge::{consult, JudgeOutcome};
    use crate::routing::serve::AdmittedContext;

    use super::judge_fixture;

    const ROUTER_ID: &str = "claude-auto-oversized-reply";
    /// A ceiling on the mock's own output, so a regression cannot turn this
    /// test into a runaway writer. Far above `judge_max_response_bytes`
    /// (64 KiB) and far below what an uncapped read would drain in two seconds.
    const MOCK_WRITE_CEILING: usize = 4 * 1024 * 1024;

    /// A judge that commits `200 application/json` and then streams a chunked
    /// body that never ends.
    ///
    /// A raw socket because no mock server can express "valid headers, then
    /// more body than anyone asked for, forever" — and the endlessness is the
    /// point: a finite oversized body is refused by the collector too, so it
    /// could not tell the two caps apart.
    ///
    /// Returns the counter of bytes it managed to write, which is the second
    /// half of the assertion: it is the memory the uncapped read would have
    /// taken.
    async fn endless_reply_judge() -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let written = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&written);
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("the judge call connects");
            let mut buffer = [0u8; 8192];
            // One read is enough to get past the request head.
            let _ = socket.read(&mut buffer).await;
            let head: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n";
            if socket.write_all(head).await.is_err() {
                return;
            }
            // 4 KiB of chunk payload at a time: enough that the 64 KiB cap is
            // crossed within the first handful, small enough that the writer
            // notices a closed peer promptly.
            let mut chunk = Vec::with_capacity(4096 + 8);
            chunk.extend_from_slice(b"1000\r\n");
            chunk.extend_from_slice(&[b'a'; 4096]);
            chunk.extend_from_slice(b"\r\n");
            while counter.load(Ordering::Relaxed) < MOCK_WRITE_CEILING {
                if socket.write_all(&chunk).await.is_err() {
                    break;
                }
                counter.fetch_add(4096, Ordering::Relaxed);
            }
        });
        (format!("http://{addr}"), written)
    }

    #[tokio::test]
    async fn an_endless_judge_reply_is_refused_at_the_cap_not_at_the_deadline() {
        let (judge_url, written) = endless_reply_judge().await;
        let stage = judge_fixture::stage();
        let state = judge_fixture::state(ROUTER_ID, judge_url, &stage);
        let classifier = stage
            .classifier
            .as_ref()
            .expect("the fixture names a judge");

        let inbound = InboundContext::internal();
        let headers = HeaderMap::new();
        let admitted = AdmittedContext::mint(&inbound, &headers, ROUTER_ID);
        let request = json!({
            "model": ROUTER_ID,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        });

        let started_at = Instant::now();
        let outcome = consult(&state, &admitted, &stage, classifier, &request).await;
        let elapsed = started_at.elapsed();

        assert_eq!(
            outcome,
            JudgeOutcome::FailOpen("oversized"),
            "the reply crossed `judge_max_response_bytes`, so that is the key \
             the operator has to read — any other outcome here means the \
             adapter kept draining the body past the cap"
        );
        assert!(
            elapsed < Duration::from_millis(2_000),
            "the refusal is the cap's, not the deadline's, but took {elapsed:?}"
        );
        // The upstream never got to hand over more than a small multiple of the
        // cap: whatever the socket buffers absorbed before the abandoned read
        // closed the connection.
        let written = written.load(Ordering::Relaxed);
        assert!(
            written < MOCK_WRITE_CEILING,
            "the whole body was still being drained: the judge wrote {written} bytes"
        );
    }

    /// A judge that announces a body far over the cap and then sends none of
    /// it. The declaration alone is enough to refuse: nothing about this reply
    /// can come in under `judge_max_response_bytes`, so waiting for bytes that
    /// would only confirm it spends the deadline to learn what the headers
    /// already said.
    ///
    /// The distinguishing case for the early check. Counting only bytes that
    /// arrive, this stalls until `judge_timeout_ms` and is reported as
    /// `timeout` — the wrong key for an operator, who would go looking for a
    /// slow judge instead of an oversized one.
    async fn declared_oversized_judge() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("the judge call connects");
            let mut buffer = [0u8; 8192];
            let _ = socket.read(&mut buffer).await;
            // 10 MiB declared against a 64 KiB cap, and not one byte of body.
            let head: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 10485760\r\n\r\n";
            if socket.write_all(head).await.is_err() {
                return;
            }
            // Hold the connection open so the only way out is the refusal.
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn a_judge_declaring_an_oversized_body_is_refused_before_it_sends_one() {
        let judge_url = declared_oversized_judge().await;
        let stage = judge_fixture::stage();
        let state = judge_fixture::state(ROUTER_ID, judge_url, &stage);
        let classifier = stage
            .classifier
            .as_ref()
            .expect("the fixture names a judge");

        let inbound = InboundContext::internal();
        let headers = HeaderMap::new();
        let admitted = AdmittedContext::mint(&inbound, &headers, ROUTER_ID);
        let request = json!({
            "model": ROUTER_ID,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        });

        let started_at = Instant::now();
        let outcome = consult(&state, &admitted, &stage, classifier, &request).await;
        let elapsed = started_at.elapsed();

        assert_eq!(
            outcome,
            JudgeOutcome::FailOpen("oversized"),
            "the reply announced more than `judge_max_response_bytes`, so it is \
             oversized and not slow — `timeout` here means the declaration was \
             ignored and the collector waited for bytes instead"
        );
        assert!(
            elapsed < Duration::from_millis(1_000),
            "the refusal comes from the headers, well inside the 2s deadline, \
             but took {elapsed:?}"
        );
    }
}
