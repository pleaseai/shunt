//! An advisor review `max_judge_calls` refused, under `fail_open = false`
//! (issue #686).
//!
//! In-crate because the judge-call sample store is `cfg(test)`, so an
//! integration binary cannot read the series this pins.
//!
//! The decision: the refusal is folded into libsy's own cascade, as on every
//! other lane (ADR-0005 §3, 2026-09-25 amendment), and an advisor under
//! `fail_open = false` fails the turn on it — a gateway-owned `502`,
//! `gated_error` — but the refusal is still recorded as `budget_exhausted`,
//! the outcome that names every refusal.

use std::collections::BTreeMap;

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use serde_json::json;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use crate::config::{AuthMode, Config, ModelConfig, ProviderConfig, RouterConfig};
use crate::server::AppState;

const ROUTER_ID: &str = "claude-advisor-budget-686";

/// A review budget of two and a judge-call budget of one: the second turn's
/// review is refused by `max_judge_calls`, not by the advisor's own ledger.
const ROUTER: &str = r#"
type = "advisor"
executor_target = "executor-alias"
advisor_target = "judge-alias"
fail_open = false
max_reviews = 2
max_judge_calls = 1
judge_timeout_ms = 2000
"#;

fn provider(base_url: String) -> ProviderConfig {
    let mut provider = Config::default()
        .providers
        .remove("anthropic")
        .expect("the default config ships an anthropic provider");
    provider.base_url = base_url;
    // Credential-injecting, so a legal judge target, with no environment.
    provider.auth = AuthMode::None;
    provider
}

fn alias(id: &str, provider: &str) -> ModelConfig {
    ModelConfig {
        subagents: None,
        id: id.to_string(),
        display_name: None,
        upstream_model: Some(BTreeMap::from([(
            provider.to_string(),
            format!("{id}-upstream"),
        )])),
        router: None,
        stage_router: None,
    }
}

fn message(text: &str) -> serde_json::Value {
    json!({
        "id": "msg_gated",
        "type": "message",
        "role": "assistant",
        "model": "upstream",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1},
    })
}

async fn turn(state: &AppState) -> (StatusCode, String) {
    let uri: axum::http::Uri = "/v1/messages".parse().unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert(
        "x-claude-code-session-id",
        HeaderValue::from_static("session-686"),
    );
    let body = axum::body::Body::from(
        json!({
            "model": ROUTER_ID,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "add a --json flag"}],
        })
        .to_string(),
    );
    let response = match super::forward(
        state.clone(),
        &uri,
        &headers,
        body,
        std::time::Instant::now(),
    )
    .await
    {
        Ok((_, response)) => response,
        Err(error) => axum::response::IntoResponse::into_response(error),
    };
    let source = response.headers()["x-gateway-route-source"]
        .to_str()
        .expect("x-gateway-route-source is ASCII")
        .to_string();
    (response.status(), source)
}

fn samples(outcome: &'static str) -> u64 {
    crate::metrics::judge_call_samples_for_tests(ROUTER_ID, "advisor", outcome)
}

/// Non-vacuity: leave `GatedDecision::Fail`'s judge outcome unread in
/// `consult` (the pre-#686 `None`) and the last assertion goes red on `0`.
/// The first turn is the twin: a review that was made records `decided` and
/// not `budget_exhausted`, so the sample below is the refusal's own.
#[tokio::test]
async fn a_budget_refused_review_under_fail_open_false_is_a_502_recorded_as_budget_exhausted() {
    let (executor, advisor) = (MockServer::start().await, MockServer::start().await);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(message("EXECUTOR-ANSWER")))
        .expect(2)
        .mount(&executor)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(message("APPROVE")))
        .expect(1)
        .mount(&advisor)
        .await;
    let mut config = Config {
        models: vec![
            ModelConfig {
                subagents: None,
                id: ROUTER_ID.to_string(),
                display_name: None,
                upstream_model: None,
                router: Some(toml::from_str::<RouterConfig>(ROUTER).expect("the router parses")),
                stage_router: None,
            },
            alias("executor-alias", "executor"),
            alias("judge-alias", "judge"),
        ],
        ..Config::default()
    };
    config.providers = BTreeMap::from([
        ("executor".to_string(), provider(executor.uri())),
        ("judge".to_string(), provider(advisor.uri())),
    ]);
    config.server.default_provider = "executor".to_string();
    let state = AppState::new(config, reqwest::Client::new()).expect("the config is valid");

    let first = turn(&state).await;
    assert_eq!(first, (StatusCode::OK, "advisor_approve".to_string()));
    assert_eq!(samples("decided"), 1);
    assert_eq!(samples("budget_exhausted"), 0);

    let second = turn(&state).await;
    assert_eq!(
        second,
        (StatusCode::BAD_GATEWAY, "gated_error".to_string()),
        "`fail_open = false` fails the turn whose review the budget refused"
    );
    assert_eq!(samples("decided"), 1, "no second review was made");
    assert_eq!(
        samples("budget_exhausted"),
        1,
        "the refused review is recorded under the series `max_judge_calls` is tuned by"
    );
}
