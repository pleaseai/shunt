//! End-to-end coverage for the opt-in `[models.stage_router]`.
//!
//! The unit tests under `src/routing/stage/` pin the scorer wiring and the
//! hysteresis rules in isolation. These pin what a real request actually
//! experiences: which upstream it lands on, which model id that upstream is
//! told, which id comes back to the client, and how a session's pin behaves
//! across turns.
//!
//! Non-vacuity: delete the `route.model` re-stamp in `routing::resolve_chain`
//! and `the_client_is_told_the_id_it_asked_for` goes red; make `read_only` commit
//! and `a_count_tokens_probe_does_not_pin_the_session` goes red; drop the pin
//! entirely and `a_pinned_capable_tier_survives_a_clean_turn` goes red; commit
//! the pin inside `stage::select` rather than parking it for
//! `StageContext::commit` — the shape this had before the write moved past
//! request admission — and `a_rejected_request_does_not_pin_the_session` goes
//! red, because the quiet turn is then held on the pin the 401 left behind.

use std::{collections::BTreeMap, io::ErrorKind, net::SocketAddr};

use reqwest::StatusCode;
use serde_json::{json, Value};
use shunt::{
    config::{
        ApiKeyHeader, AuthMap, AuthMode, Config, CountTokens, InboundAuthConfig, ModelConfig,
        ProviderKind, RetryConfig, StageRouterConfig, StageRouterPicker, UpstreamAuth,
        UpstreamConfig,
    },
    server,
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{method, path},
    Match, Mock, MockServer, Request, ResponseTemplate,
};

mod common;

const ROUTER_ID: &str = "claude-auto";
const CAPABLE_UPSTREAM_MODEL: &str = "upstream-capable";
const EFFICIENT_UPSTREAM_MODEL: &str = "upstream-efficient";
const SESSION: &str = "0199a0f2-2f4b-7c3e-9d61-4f1a2b3c4d5e";

struct TestGateway {
    base_url: String,
    task: JoinHandle<()>,
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Matches the outbound request body's `model`, which is how the tier the
/// router picked becomes observable at the upstream.
struct ModelIs(&'static str);

impl Match for ModelIs {
    fn matches(&self, request: &Request) -> bool {
        serde_json::from_slice::<Value>(&request.body)
            .ok()
            .and_then(|body| body.get("model")?.as_str().map(ToOwned::to_owned))
            .is_some_and(|model| model == self.0)
    }
}

fn can_bind_loopback() -> bool {
    match std::net::TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => {
            drop(listener);
            true
        }
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            eprintln!("skipping network integration test: loopback bind is not permitted");
            false
        }
        Err(error) => panic!("unexpected loopback bind failure: {error}"),
    }
}

fn passthrough(name: &str, base_url: String) -> UpstreamConfig {
    upstream_with(
        name,
        base_url,
        UpstreamAuth::Shorthand(AuthMode::Passthrough),
    )
}

fn upstream_with(name: &str, base_url: String, auth: UpstreamAuth) -> UpstreamConfig {
    UpstreamConfig {
        name: name.to_string(),
        provider: None,
        kind: Some(ProviderKind::Anthropic),
        base_url: Some(base_url),
        auth: Some(auth),
        effort: None,
        service_tier: None,
        classifier_model: None,
        count_tokens: CountTokens::Tiktoken,
        websocket: false,
        tool_search: None,
        request_compression: true,
        retry: RetryConfig {
            max_retries: 0,
            ..RetryConfig::default()
        },
        workspace_roots: Vec::new(),
        profile_dir: None,
        sandbox: true,
    }
}

fn tier_alias(id: &str, upstream: &str, upstream_model: &str) -> ModelConfig {
    ModelConfig {
        id: id.to_string(),
        display_name: None,
        upstream_model: Some(BTreeMap::from([(
            upstream.to_string(),
            upstream_model.to_string(),
        )])),
        stage_router: None,
    }
}

fn router_config(capable: &MockServer, efficient: &MockServer) -> Config {
    router_config_with(capable, efficient, passthrough)
}

fn router_config_with(
    capable: &MockServer,
    efficient: &MockServer,
    upstream: impl Fn(&str, String) -> UpstreamConfig,
) -> Config {
    let mut config = Config::default();
    config.providers.clear();
    config.upstreams = vec![
        upstream("capable", capable.uri()),
        upstream("efficient", efficient.uri()),
    ];
    config.server.default_provider = "efficient".to_string();
    config.models = vec![
        ModelConfig {
            id: ROUTER_ID.to_string(),
            display_name: None,
            upstream_model: None,
            stage_router: Some(StageRouterConfig {
                capable_target: "capable-alias".to_string(),
                efficient_target: "efficient-alias".to_string(),
                picker: StageRouterPicker::EfficientFirst,
                confidence_threshold: 0.5,
                recent_turn_window: 3,
                // Two turns, so a test can outlast the window without sending a
                // dozen requests.
                min_dwell_turns: 2,
                deescalate_threshold: None,
                session_ttl_seconds: 3600,
            }),
        },
        tier_alias("capable-alias", "capable", CAPABLE_UPSTREAM_MODEL),
        tier_alias("efficient-alias", "efficient", EFFICIENT_UPSTREAM_MODEL),
    ];
    config.validate().expect("the router config is well formed")
}

async fn start_gateway(mut config: Config) -> TestGateway {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _, _) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    TestGateway {
        base_url: format!("http://{addr}"),
        task,
    }
}

/// A conversation whose recent turns are failing investigation.
fn erroring_messages() -> Value {
    json!([
        {"role": "assistant", "content": [{"type": "tool_use", "id": "a", "name": "Read"}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a", "is_error": true}]},
        {"role": "assistant", "content": [{"type": "tool_use", "id": "b", "name": "Grep"}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "b", "is_error": true}]},
    ])
}

/// A conversation with no tool activity at all: nothing for the scorer to read.
fn quiet_messages() -> Value {
    json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}])
}

async fn post(gateway: &TestGateway, messages: Value) -> reqwest::Response {
    post_to(gateway, "/v1/messages", messages).await
}

async fn post_to(gateway: &TestGateway, path: &str, messages: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}{path}", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", SESSION)
        .body(json!({"model": ROUTER_ID, "max_tokens": 16, "messages": messages}).to_string())
        .send()
        .await
        .unwrap()
}

async fn post_with_token(
    gateway: &TestGateway,
    messages: Value,
    token: Option<&str>,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", SESSION);
    if let Some(token) = token {
        request = request.header("x-shunt-token", token);
    }
    request
        .body(json!({"model": ROUTER_ID, "max_tokens": 16, "messages": messages}).to_string())
        .send()
        .await
        .unwrap()
}

fn tier_reply(upstream_model: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string(
        json!({"id": "msg_1", "type": "message", "model": upstream_model}).to_string(),
    )
}

fn tier_mock(upstream_model: &'static str, expect: u64) -> Mock {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(ModelIs(upstream_model))
        .respond_with(tier_reply(upstream_model))
        .expect(expect)
}

/// The whole point of the router: a session that is failing gets the strong tier.
#[tokio::test]
async fn an_erroring_session_lands_on_the_capable_upstream() {
    if !can_bind_loopback() {
        return;
    }
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 1).mount(&capable).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&efficient)
        .await;
    let gateway = start_gateway(router_config(&capable, &efficient)).await;

    let response = post(&gateway, erroring_messages()).await;

    assert_eq!(response.status(), StatusCode::OK);
    capable.verify().await;
    efficient.verify().await;
}

/// The picker's default serves a session the scorer has nothing to say about.
#[tokio::test]
async fn a_quiet_session_stays_on_the_efficient_upstream() {
    if !can_bind_loopback() {
        return;
    }
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&capable)
        .await;
    tier_mock(EFFICIENT_UPSTREAM_MODEL, 1)
        .mount(&efficient)
        .await;
    let gateway = start_gateway(router_config(&capable, &efficient)).await;

    let response = post(&gateway, quiet_messages()).await;

    assert_eq!(response.status(), StatusCode::OK);
    capable.verify().await;
    efficient.verify().await;
}

/// The upstream reports its own model id. What reaches the client must be the id
/// the client asked for — Claude Code records it and restores the model from it
/// on `--resume` (issue #172).
#[tokio::test]
async fn the_client_is_told_the_id_it_asked_for() {
    if !can_bind_loopback() {
        return;
    }
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 1).mount(&capable).await;
    let gateway = start_gateway(router_config(&capable, &efficient)).await;

    let response = post(&gateway, erroring_messages()).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-gateway-model"], ROUTER_ID);
    assert_eq!(
        response.headers()["x-gateway-upstream-model"],
        CAPABLE_UPSTREAM_MODEL,
        "the tier the router picked still travels upstream"
    );
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body["model"], ROUTER_ID,
        "the upstream's own id must not reach the client"
    );
}

/// Hysteresis, end to end: once a session is pinned to the capable tier, a
/// single calm turn does not drop it back.
#[tokio::test]
async fn a_pinned_capable_tier_survives_a_clean_turn() {
    if !can_bind_loopback() {
        return;
    }
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 2).mount(&capable).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&efficient)
        .await;
    let gateway = start_gateway(router_config(&capable, &efficient)).await;

    assert_eq!(
        post(&gateway, erroring_messages()).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        post(&gateway, quiet_messages()).await.status(),
        StatusCode::OK,
        "a quiet turn is not evidence, so the pin holds"
    );

    capable.verify().await;
    efficient.verify().await;
}

/// Claude Code sends `count_tokens` with a history one turn behind. The probe
/// must reach the same tier as the turn it is measuring — an estimate counted
/// against the wrong model is the wrong estimate — without recording it, or the
/// stale history drives the session's pin.
#[tokio::test]
async fn a_count_tokens_probe_does_not_pin_the_session() {
    if !can_bind_loopback() {
        return;
    }
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    // These upstreams are Anthropic-kind, so `count_tokens` is forwarded rather
    // than counted locally — which is what makes the probe's tier observable.
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .and(ModelIs(CAPABLE_UPSTREAM_MODEL))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"input_tokens":7}"#))
        .expect(1)
        .mount(&capable)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&capable)
        .await;
    tier_mock(EFFICIENT_UPSTREAM_MODEL, 1)
        .mount(&efficient)
        .await;
    let gateway = start_gateway(router_config(&capable, &efficient)).await;

    let probe = post_to(&gateway, "/v1/messages/count_tokens", erroring_messages()).await;
    assert_eq!(probe.status(), StatusCode::OK);

    // Had the probe committed, this quiet turn would be held on capable.
    let response = post(&gateway, quiet_messages()).await;
    assert_eq!(response.status(), StatusCode::OK);

    capable.verify().await;
    efficient.verify().await;
}

/// A request the gateway refuses must not leave a pin behind.
///
/// Routing has to run before `check_inbound_auth` — that gate reads the resolved
/// chain to decide whether the route injects a credential — so the tier is
/// chosen while the caller is still unauthenticated. Recording it there let an
/// unauthenticated caller pin, evict, and steer a session it never proved it
/// owns; the write now waits until the request has been admitted.
#[tokio::test]
async fn a_rejected_request_does_not_pin_the_session() {
    if !can_bind_loopback() {
        return;
    }
    let mut vars = common::env_lock().await;
    vars.set("SHUNT_TEST_STAGE_KEY", "upstream-key");
    vars.set("SHUNT_TEST_STAGE_TOKENS", "alice:tok-a");

    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    // The rejected turn is the erroring one, so if it pinned it would pin
    // *capable* — and the quiet turn afterwards would be held there by the dwell
    // window instead of being scored on its own.
    tier_mock(CAPABLE_UPSTREAM_MODEL, 0).mount(&capable).await;
    tier_mock(EFFICIENT_UPSTREAM_MODEL, 1)
        .mount(&efficient)
        .await;

    // Inbound auth only gates a route that injects a credential, so both tiers
    // have to carry one for this test to reach the gate at all.
    let mut config = router_config_with(&capable, &efficient, |name, uri| {
        upstream_with(
            name,
            uri,
            UpstreamAuth::Map(AuthMap::ApiKey {
                env: Some("SHUNT_TEST_STAGE_KEY".to_string()),
                header: Some(ApiKeyHeader::Bearer),
            }),
        )
    });
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: "SHUNT_TEST_STAGE_TOKENS".to_string(),
    });
    let config = config.validate().expect("the gated router config is valid");
    let gateway = start_gateway(config).await;

    let rejected = post_with_token(&gateway, erroring_messages(), None).await;
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);

    // Had the rejected turn pinned, this quiet one would be held on capable.
    let served = post_with_token(&gateway, quiet_messages(), Some("tok-a")).await;
    assert_eq!(served.status(), StatusCode::OK);

    capable.verify().await;
    efficient.verify().await;
}

/// The two observability headers name the tier and the reason.
///
/// They exist because the decision is otherwise invisible to the client:
/// `x-gateway-model` is deliberately the id the caller asked for, and
/// `x-gateway-upstream-model` is the name sent upstream, so neither says which
/// configured target the router picked.
///
/// Non-vacuity: drop the `stage_stamp` argument at the success call site in
/// `stamp_gateway_headers` and both assertions go red on a missing header.
#[tokio::test]
async fn a_routed_response_names_the_tier_and_the_reason() {
    if !can_bind_loopback() {
        return;
    }
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    tier_mock(CAPABLE_UPSTREAM_MODEL, 1).mount(&capable).await;
    let gateway = start_gateway(router_config(&capable, &efficient)).await;

    let response = post(&gateway, erroring_messages()).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["x-gateway-routed-model"],
        "capable-alias",
        "the header names the configured target, not the upstream model name"
    );
    assert_ne!(
        response.headers()["x-gateway-routed-model"],
        response.headers()["x-gateway-upstream-model"],
        "the two headers must be distinguishable, or the new one says nothing"
    );
    // Two trailing failures are scored as critical, which libsy reports as a
    // hard override rather than a dimension score.
    assert_eq!(response.headers()["x-gateway-route-source"], "override");
}

/// A model id with no `[models.stage_router]` table must not gain the headers:
/// a client has to be able to tell "routed to a tier" from "not router-routed".
///
/// Non-vacuity: stamp the pair unconditionally — with an empty value, say —
/// and this goes red.
#[tokio::test]
async fn an_unrouted_response_carries_no_stage_headers() {
    if !can_bind_loopback() {
        return;
    }
    let capable = MockServer::start().await;
    let efficient = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(tier_reply(EFFICIENT_UPSTREAM_MODEL))
        .mount(&efficient)
        .await;
    let gateway = start_gateway(router_config(&capable, &efficient)).await;

    // `efficient-alias` is an ordinary `[[models]]` entry in this config.
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", SESSION)
        .body(
            json!({"model": "efficient-alias", "max_tokens": 16, "messages": quiet_messages()})
                .to_string(),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !response.headers().contains_key("x-gateway-routed-model"),
        "an ordinary model id must not report a routed tier"
    );
    assert!(
        !response.headers().contains_key("x-gateway-route-source"),
        "an ordinary model id must not report a decision source"
    );
    assert_eq!(
        response.headers()["x-gateway-model"],
        "efficient-alias",
        "the ordinary headers are unaffected"
    );
}
