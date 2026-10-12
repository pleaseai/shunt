//! A judge reply the upstream cut short is a transport fault, not an answer
//! and not a malformed one (issue #635).
//!
//! A Responses judge whose upstream stops before `response.completed` is
//! answered by its adapter with a whole-looking message carrying every delta
//! that arrived, marked `UpstreamTruncated`. Those bytes parse, so the only
//! thing that tells the cut apart is the mark. Here the verdict itself arrived
//! whole before the cut, which is the sharpest form: read as an answer, the
//! consult *decides* on it.

use std::collections::BTreeMap;

use axum::http::HeaderMap;
use serde_json::json;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

use crate::config::{
    AuthMode, Config, ModelConfig, RouterConfig, StageClassifierConfig, StageRouterConfig,
    StageRouterPicker,
};
use crate::proxy::failover::InboundContext;
use crate::routing::judge::{consult, JudgeOutcome};
use crate::routing::serve::AdmittedContext;
use crate::routing::stage::StageTier;
use crate::server::AppState;

const ROUTER_ID: &str = "claude-auto-truncated-judge";

/// A verdict a `stage_router` judge decides on: `p_solve` under the threshold
/// declines the efficient tier.
fn verdict_delta() -> String {
    let crux = json!({
        "crux": "bounded task",
        "primary_rule": "SUP-1",
        "capability_boundary": "supported",
        "p_solve": 0.1,
    });
    json!({"delta": crux.to_string()}).to_string()
}

/// The judge's Responses stream, ending on `response.completed` or not.
fn judge_stream(completed: bool) -> String {
    let mut sse = format!(
        "event: response.created\ndata: {{\"response\":{{\"id\":\"resp_1\",\"usage\":{{\"output_tokens\":0}}}}}}\n\n\
         event: response.output_text.delta\ndata: {}\n\n",
        verdict_delta()
    );
    if completed {
        sse.push_str(
            "event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\ndata: [DONE]\n\n",
        );
    }
    sse
}

/// A `stage_router` entry whose judge is an api-key Responses provider on
/// `judge_url`.
fn state(stage: &StageRouterConfig, judge_url: String) -> AppState {
    let mut config = Config {
        models: vec![
            ModelConfig {
                subagents: None,
                id: ROUTER_ID.to_string(),
                display_name: None,
                upstream_model: None,
                router: Some(RouterConfig::StageRouter(stage.clone())),
                stage_router: None,
            },
            ModelConfig {
                subagents: None,
                id: "judge-alias".to_string(),
                display_name: None,
                upstream_model: Some(BTreeMap::from([(
                    "openai".to_string(),
                    "upstream-judge".to_string(),
                )])),
                router: None,
                stage_router: None,
            },
        ],
        ..Config::default()
    };
    let openai = config.providers.get_mut("openai").unwrap();
    openai.base_url = judge_url;
    // Credential-injecting without an environment variable.
    openai.auth = AuthMode::None;
    config.server.default_provider = "openai".to_string();
    AppState::new(config, reqwest::Client::new()).expect("the config is valid")
}

async fn consult_judge(completed: bool) -> JudgeOutcome {
    let judge = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(judge_stream(completed)),
        )
        .expect(1)
        .mount(&judge)
        .await;
    let stage = StageRouterConfig {
        classifier: Some(StageClassifierConfig {
            target: "judge-alias".to_string(),
            base_threshold: 0.5,
            classify_trigger: Default::default(),
        }),
        judge_timeout_ms: 2_000,
        ..StageRouterConfig::preset(
            "capable-alias".to_string(),
            "efficient-alias".to_string(),
            StageRouterPicker::EfficientFirst,
            0.5,
        )
    };
    let state = state(&stage, judge.uri());
    let inbound = InboundContext::internal();
    let headers = HeaderMap::new();
    let admitted = AdmittedContext::mint(&inbound, &headers, ROUTER_ID);
    let request = json!({
        "model": ROUTER_ID,
        "max_tokens": 16,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
    });
    let classifier = stage.classifier.as_ref().expect("the entry names a judge");
    consult(&state, &admitted, &stage, classifier, &request).await
}

/// Non-vacuity: drop the `UpstreamTruncated` check in `super::dispatch` and
/// this comes back `Decided(Capable)` — the cut reply accepted as a verdict.
#[tokio::test]
async fn a_judge_reply_cut_before_its_terminal_event_is_an_upstream_error() {
    assert_eq!(
        consult_judge(false).await,
        JudgeOutcome::FailOpen("upstream_error"),
        "a reply the upstream never finished is a transport fault: neither a \
         verdict nor `invalid_reply`"
    );
}

/// The positive twin: the same verdict with its terminal event is decided,
/// so the refusal above is the cut's and not the fixture's.
#[tokio::test]
async fn the_same_judge_reply_with_its_terminal_event_is_decided() {
    assert_eq!(
        consult_judge(true).await,
        JudgeOutcome::Decided(StageTier::Capable)
    );
}
