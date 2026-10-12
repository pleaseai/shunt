//! End-to-end coverage for `[models.subagents]` in its `passthrough` form
//! (ADR-0005 §5, §11 — PR 3 of the §8 sequence).
//!
//! The unit tests in `src/routing/subagents.rs` pin the predicate per request
//! class. These pin what a real request experiences through the axum router,
//! the inbound-auth gate, and adapter dispatch: which upstream a delegated
//! turn lands on, which one the parent keeps, and which headers say why. Each
//! upstream is its own mock server with an exact call expectation, so a turn
//! that leaks to the wrong destination fails on that server's count rather
//! than passing by accident.
//!
//! Non-vacuity: delete the overlay arm at the top of `routing::resolve_chain`
//! and every `*_lands_on_*` test here goes red on the mock expectations;
//! make `RouterContext::is_delegated` ignore the class and
//! `main_with_an_agent_id_stays_on_the_parent` goes red.

use std::{collections::BTreeMap, io::ErrorKind, net::SocketAddr};

use reqwest::StatusCode;
use serde_json::json;
use shunt::{
    config::{
        AuthMode, Config, CountTokens, ModelConfig, ProviderKind, RetryConfig, SubagentsConfig,
        UpstreamAuth, UpstreamConfig,
    },
    server,
};
use tokio::task::JoinHandle;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};

mod common;

const HOST_ID: &str = "claude-main";
const SESSION: &str = "0f0a2cc3-d5f1-4200-b9c8-f56a081194ce";
/// A captured child id (`docs/notes/adr-0005-routing-live-captures.md`, fact (a)).
const AGENT_ID: &str = "a7a11c2e22e29e67a";

struct TestGateway {
    base_url: String,
    task: JoinHandle<()>,
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.task.abort();
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

fn upstream(name: &str, base_url: String) -> UpstreamConfig {
    UpstreamConfig {
        name: name.to_string(),
        provider: None,
        kind: Some(ProviderKind::Anthropic),
        base_url: Some(base_url),
        auth: Some(UpstreamAuth::Shorthand(AuthMode::Passthrough)),
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

fn mapped(id: &str, upstream: &str, upstream_model: &str) -> ModelConfig {
    ModelConfig {
        id: id.to_string(),
        display_name: None,
        upstream_model: Some(BTreeMap::from([(
            upstream.to_string(),
            upstream_model.to_string(),
        )])),
        router: None,
        subagents: None,
        stage_router: None,
    }
}

/// Upstream's "passthrough with subagents": a fixed entry whose delegated turns
/// go elsewhere. Three upstreams, one per destination.
struct Upstreams {
    parent: MockServer,
    child: MockServer,
    explorer: MockServer,
}

impl Upstreams {
    async fn start(parent_calls: u64, child_calls: u64, explorer_calls: u64) -> Self {
        let upstreams = Self {
            parent: MockServer::start().await,
            child: MockServer::start().await,
            explorer: MockServer::start().await,
        };
        for (server, name, calls) in [
            (&upstreams.parent, "parent", parent_calls),
            (&upstreams.child, "child", child_calls),
            (&upstreams.explorer, "explorer", explorer_calls),
        ] {
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_string(
                    json!({"id": "msg_1", "type": "message", "model": name}).to_string(),
                ))
                .expect(calls)
                .mount(server)
                .await;
        }
        upstreams
    }

    fn config(&self) -> Config {
        let overlay: SubagentsConfig = toml::from_str(
            r#"
            type = "passthrough"
            target = "child-alias"
            by_type = { Explore = "explorer-alias" }
            "#,
        )
        .expect("the overlay parses");
        let mut config = Config::default();
        config.providers.clear();
        config.upstreams = vec![
            upstream("parent", self.parent.uri()),
            upstream("child", self.child.uri()),
            upstream("explorer", self.explorer.uri()),
        ];
        config.server.default_provider = "parent".to_string();
        let mut host = mapped(HOST_ID, "parent", "upstream-parent");
        host.subagents = Some(overlay);
        config.models = vec![
            host,
            mapped("child-alias", "child", "upstream-child"),
            mapped("explorer-alias", "explorer", "upstream-explorer"),
        ];
        config
            .validate()
            .expect("the overlay config is well formed")
    }
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

/// One `/v1/messages` turn for `HOST_ID` with the given `x-claude-code-*` hints.
async fn post(gateway: &TestGateway, hints: &[(&str, &str)]) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", SESSION);
    for (name, value) in hints {
        request = request.header(*name, *value);
    }
    request
        .body(
            json!({
                "model": HOST_ID,
                "max_tokens": 16,
                "messages": [{"role": "user", "content": "hi"}],
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap()
}

/// Clause 1, `by_type`: an `Explore` child, with the hint gate on, lands on
/// the upstream its `by_type` entry names — and the response says so.
#[tokio::test]
async fn an_explore_child_lands_on_its_by_type_upstream() {
    if !can_bind_loopback() {
        return;
    }
    // The config build below reads the environment, and `setenv` in a
    // concurrent test can make an unrelated `getenv` look empty — so a
    // reader takes the shared guard too (tests/AGENTS.md).
    let _env = common::env_lock().await;
    let upstreams = Upstreams::start(0, 0, 1).await;
    let gateway = start_gateway(upstreams.config()).await;

    let response = post(
        &gateway,
        &[
            ("x-claude-code-agent-id", AGENT_ID),
            ("x-claude-code-agent-type", "Explore"),
            ("x-claude-code-request-class", "subagent"),
        ],
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["x-gateway-model"],
        HOST_ID,
        "the client is told the id it asked for"
    );
    assert_eq!(
        response.headers()["x-gateway-upstream-model"],
        "upstream-explorer"
    );
    assert_eq!(
        response.headers()["x-gateway-routed-model"],
        "explorer-alias"
    );
    assert_eq!(
        response.headers()["x-gateway-route-source"],
        "subagent_type"
    );
}

/// Clause 1, `target`: the default deployment sends the agent id and nothing
/// else — no class, no type — and the child lands on `target`.
#[tokio::test]
async fn a_child_without_a_by_type_entry_lands_on_target() {
    if !can_bind_loopback() {
        return;
    }
    // The config build below reads the environment, and `setenv` in a
    // concurrent test can make an unrelated `getenv` look empty — so a
    // reader takes the shared guard too (tests/AGENTS.md).
    let _env = common::env_lock().await;
    let upstreams = Upstreams::start(0, 2, 0).await;
    let gateway = start_gateway(upstreams.config()).await;

    // Gate off: the agent id alone.
    let response = post(&gateway, &[("x-claude-code-agent-id", AGENT_ID)]).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-gateway-routed-model"], "child-alias");
    assert_eq!(response.headers()["x-gateway-route-source"], "subagent");

    // Gate on, a type `by_type` does not name.
    let response = post(
        &gateway,
        &[
            ("x-claude-code-agent-id", AGENT_ID),
            ("x-claude-code-agent-type", "general-purpose"),
            ("x-claude-code-request-class", "workflow"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["x-gateway-upstream-model"],
        "upstream-child"
    );
    assert_eq!(response.headers()["x-gateway-route-source"], "subagent");
}

/// Clause 2: the class is authoritative. `main` with an agent id — and a type
/// `by_type` would match — is main traffic and keeps the parent's destination,
/// with none of the router headers a diverted turn carries.
#[tokio::test]
async fn main_with_an_agent_id_stays_on_the_parent() {
    if !can_bind_loopback() {
        return;
    }
    // The config build below reads the environment, and `setenv` in a
    // concurrent test can make an unrelated `getenv` look empty — so a
    // reader takes the shared guard too (tests/AGENTS.md).
    let _env = common::env_lock().await;
    let upstreams = Upstreams::start(1, 0, 0).await;
    let gateway = start_gateway(upstreams.config()).await;

    let response = post(
        &gateway,
        &[
            ("x-claude-code-agent-id", AGENT_ID),
            ("x-claude-code-agent-type", "Explore"),
            ("x-claude-code-request-class", "main"),
        ],
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["x-gateway-upstream-model"],
        "upstream-parent"
    );
    assert!(!response.headers().contains_key("x-gateway-routed-model"));
    assert!(!response.headers().contains_key("x-gateway-route-source"));
}

/// Clause 2: `compaction` and `auxiliary` are harness maintenance and never
/// take the overlay, whatever else the request carries.
#[tokio::test]
async fn compaction_and_auxiliary_stay_on_the_parent() {
    if !can_bind_loopback() {
        return;
    }
    // The config build below reads the environment, and `setenv` in a
    // concurrent test can make an unrelated `getenv` look empty — so a
    // reader takes the shared guard too (tests/AGENTS.md).
    let _env = common::env_lock().await;
    let upstreams = Upstreams::start(2, 0, 0).await;
    let gateway = start_gateway(upstreams.config()).await;

    for class in ["compaction", "auxiliary"] {
        let response = post(
            &gateway,
            &[
                ("x-claude-code-agent-id", AGENT_ID),
                ("x-claude-code-agent-type", "Explore"),
                ("x-claude-code-request-class", class),
            ],
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK, "{class}");
        assert_eq!(
            response.headers()["x-gateway-upstream-model"],
            "upstream-parent",
            "{class} must keep the parent's destination"
        );
        assert!(
            !response.headers().contains_key("x-gateway-route-source"),
            "{class} is not a routed turn"
        );
    }
}
