//! Bound tests for the internal-call collectors (ADR-0005 §3, issue #594).
//!
//! Each test names the bound it is about, because the point of a closed
//! [`BoundExceeded`] set is that an operator can read the failure back to the
//! key they wrote. Non-vacuity: make [`collect_bounded`] buffer first and check
//! afterwards and `an_oversized_body_is_refused` still passes — which is why it
//! asserts the *cap* it reports rather than only that it failed; delete the
//! ping check in `is_ping_only` and
//! `a_ping_only_chunk_does_not_reset_the_idle_gap` goes red, because the
//! keep-alive stream then runs forever; drop the `hard_deadline` comparison and
//! `a_stream_past_its_wall_clock_bound_reports_duration` reports `Idle`.

use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;

use super::bounds::{bound_stream, collect_bounded, BoundExceeded, GatedBounds};

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
    assert_eq!(
        error.max_bytes, 9,
        "the refusal names the cap that was crossed, not the body's size"
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

/// The judge's upstream call is an ordinary proxied request, separated from the
/// client turn it was made for by one attribute.
///
/// In-crate rather than in `tests/router_judge.rs` because the sample store is
/// `cfg(test)` and an integration binary links the library without it. Drop the
/// `caller` argument at `run_chain`'s call site in [`super::dispatch`] and this
/// goes red on a sample filed under `client`.
mod caller_attribution {
    use std::collections::BTreeMap;

    use axum::http::HeaderMap;
    use serde_json::json;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use crate::config::{
        AuthMode, Config, ModelConfig, ProviderConfig, RouterConfig, StageClassifierConfig,
        StageRouterConfig, StageRouterPicker,
    };
    use crate::proxy::failover::InboundContext;
    use crate::routing::judge::{consult, JudgeOutcome};
    use crate::routing::serve::AdmittedContext;
    use crate::routing::stage::StageTier;
    use crate::server::AppState;

    const ROUTER_ID: &str = "claude-auto-caller-metric";

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

        let stage = StageRouterConfig {
            classifier: Some(StageClassifierConfig {
                target: "judge-alias".to_string(),
                base_threshold: 0.5,
            }),
            ..StageRouterConfig::preset(
                "capable-alias".to_string(),
                "efficient-alias".to_string(),
                StageRouterPicker::EfficientFirst,
                0.5,
            )
        };
        let mut config = Config {
            models: vec![
                ModelConfig {
                    id: ROUTER_ID.to_string(),
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
        config.providers = BTreeMap::from([("judge".to_string(), provider(judge.uri()))]);
        config.server.default_provider = "judge".to_string();
        let state = AppState::new(config, reqwest::Client::new()).expect("the config is valid");

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
