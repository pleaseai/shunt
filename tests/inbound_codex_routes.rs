//! Opt-in per-model routing on the inbound Codex endpoint
//! (`[[server.codex_endpoint.routes]]`, issue #436) — the companion to
//! `tests/inbound_codex_endpoint.rs`, which covers the fixed-provider mode.
//!
//! The behaviors under test are the ones that separate a routed request from
//! the fixed passthrough: the body `model` is rewritten to the route's
//! `upstream_model`, a third-party upstream receives a **fresh header
//! allowlist** (no client secret, no Codex-CLI identity, no shunt slot) and an
//! identity-encoded body, its response — success, SSE, or 429 — relays
//! verbatim with no rotation, and a model with no route still takes the fixed
//! pool path byte-for-byte.

use std::{io::ErrorKind, net::SocketAddr};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::StatusCode;
use shunt::{
    config::{AccountConfig, CodexEndpointConfig, CodexRouteConfig, Config, InboundAuthConfig},
    server,
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{body_string, header, method, path},
    Match, Mock, MockServer, Request, ResponseTemplate,
};

/// A raw OpenAI Responses request body, exactly as the Codex CLI would send it.
/// `model` is first so a rewrite is visibly a JSON edit and not a text splice.
fn inbound_body(model: &str) -> String {
    format!(
        r#"{{"model":"{model}","instructions":"be brief","input":[{{"type":"message","role":"user","content":[{{"type":"input_text","text":"hi"}}]}}],"stream":false,"store":false}}"#
    )
}

const FAR_FUTURE_EXP: u64 = 4_102_444_800;

struct BearerToken(String);

impl Match for BearerToken {
    fn matches(&self, request: &Request) -> bool {
        request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            == Some(format!("Bearer {}", self.0).as_str())
    }
}

/// Asserts a header the routed allowlist must drop never reaches the upstream.
struct HeaderAbsent(&'static str);

impl Match for HeaderAbsent {
    fn matches(&self, request: &Request) -> bool {
        !request.headers.contains_key(self.0)
    }
}

struct TestGateway {
    base_url: String,
    task: JoinHandle<()>,
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A name-only, `token_env`-backed pool entry (Codex accounts carry no `uuid`).
fn account(name: &str, token_env: &str) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        token_env: Some(token_env.to_string()),
        ..Default::default()
    }
}

/// Fake ChatGPT access token carrying the `chatgpt_account_id` claim shunt reads.
fn chatgpt_token(exp: u64, account_id: &str) -> String {
    let payload = serde_json::json!({
        "exp": exp,
        "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
    });
    format!(
        "x.{}.y",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    )
}

fn route(model: &str, provider: &str, upstream_model: Option<&str>) -> CodexRouteConfig {
    CodexRouteConfig {
        model: model.to_string(),
        provider: provider.to_string(),
        upstream_model: upstream_model.map(ToOwned::to_owned),
    }
}

/// A config with two upstreams behind the inbound endpoint: the built-in
/// `codex` provider (the fixed, `chatgpt_oauth` pool) and the built-in `openai`
/// provider (`auth = "api_key"`, standing in for a GLM/DeepSeek-style native
/// Responses upstream), plus whatever routes the test declares.
fn test_config(
    codex_url: &str,
    codex_token_env: &str,
    third_party_url: &str,
    third_party_key_env: &str,
    routes: Vec<CodexRouteConfig>,
) -> Config {
    let mut config = Config::default();
    let codex = config.providers.get_mut("codex").unwrap();
    codex.base_url = codex_url.to_string();
    codex.accounts = vec![account("pool-account", codex_token_env)];
    let third_party = config.providers.get_mut("openai").unwrap();
    third_party.base_url = third_party_url.to_string();
    third_party.api_key_env = Some(third_party_key_env.to_string());
    config.server.codex_endpoint = Some(CodexEndpointConfig {
        provider: "codex".to_string(),
        routes,
    });
    config
}

/// [`test_config`] plus a second `chatgpt_oauth` provider (`codex-work`, cloned
/// from the `codex` preset) that `route` points at.
fn second_chatgpt_config(
    codex_url: &str,
    codex_token_env: &str,
    third_party_key_env: &str,
    work_url: &str,
    work_token_env: &str,
    route: CodexRouteConfig,
) -> Config {
    let mut config = test_config(
        codex_url,
        codex_token_env,
        "http://127.0.0.1:1",
        third_party_key_env,
        vec![route],
    );
    let mut work_provider = config.providers.get("codex").unwrap().clone();
    work_provider.base_url = work_url.to_string();
    work_provider.accounts = vec![account("work-account", work_token_env)];
    config
        .providers
        .insert("codex-work".to_string(), work_provider);
    config
}

async fn start_gateway_with(mut config: Config) -> TestGateway {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, state) = server::build_router(config).unwrap();
    shunt::state_persist::restore(&state).await;
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    TestGateway {
        base_url: format!("http://{addr}"),
        task,
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

/// POST a raw Responses request carrying the full set of headers a real Codex
/// CLI sends, so the routed allowlist is exercised against the real shape.
async fn post_responses(gateway: &TestGateway, body: String) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/backend-api/codex/responses", gateway.base_url))
        .header("content-type", "application/json")
        // A caller's own credential: it must never reach a third-party host.
        .header("authorization", "Bearer client-would-be-forwarded")
        .header("x-api-key", "client-api-key")
        .header("chatgpt-account-id", "client-account")
        .header("originator", "codex_cli_rs")
        .header("version", "0.153.3")
        .header("session-id", "sess-routed")
        .header("x-codex-window-id", "sess-routed:0")
        // The CLI's own routing hint, naming the *public* model it asked for.
        .header("x-codex-routing-hint", "model=routing-hint-probe")
        .header("x-shunt-token", "client-shunt-token")
        .body(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn routed_model_is_rewritten_and_sent_to_the_third_party_with_its_api_key() {
    if !can_bind_loopback() {
        return;
    }
    std::env::set_var(
        "SHUNT_TEST_ROUTED_CODEX_A",
        chatgpt_token(FAR_FUTURE_EXP, "acct-a"),
    );
    std::env::set_var("SHUNT_TEST_ROUTED_KEY_A", "third-party-key");

    let upstream_body = r#"{"id":"resp_routed","object":"response","status":"completed"}"#;
    let third_party = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(BearerToken("third-party-key".to_string()))
        .and(HeaderAbsent("chatgpt-account-id"))
        .and(HeaderAbsent("originator"))
        .and(HeaderAbsent("version"))
        .and(HeaderAbsent("session-id"))
        .and(HeaderAbsent("x-codex-window-id"))
        .and(HeaderAbsent("x-shunt-token"))
        .and(HeaderAbsent("x-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(upstream_body, "application/json"))
        .expect(1)
        .mount(&third_party)
        .await;
    let codex = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&codex)
        .await;

    let gateway = start_gateway_with(test_config(
        &codex.uri(),
        "SHUNT_TEST_ROUTED_CODEX_A",
        &third_party.uri(),
        "SHUNT_TEST_ROUTED_KEY_A",
        vec![route("glm-5.3", "openai", Some("gpt-5.6-sol"))],
    ))
    .await;

    let response = post_responses(&gateway, inbound_body("glm-5.3")).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), upstream_body);

    let requests = third_party.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    // The route's `upstream_model` replaces the client's id...
    assert_eq!(sent["model"], "gpt-5.6-sol");
    // ...and nothing else about the turn changes.
    assert_eq!(sent["instructions"], "be brief");
    assert_eq!(sent["stream"], false);
    assert_eq!(sent["store"], false);
    assert_eq!(sent["input"][0]["content"][0]["text"], "hi");
    third_party.verify().await;
    codex.verify().await;
}

#[tokio::test]
async fn unrouted_model_still_uses_the_fixed_pool_verbatim() {
    // The positive twin of the test above, on the same gateway config: a model
    // with no route must keep today's behavior exactly — the fixed pool, the
    // pool bearer + `chatgpt-account-id`, and a byte-identical body.
    if !can_bind_loopback() {
        return;
    }
    let token = chatgpt_token(FAR_FUTURE_EXP, "acct-b");
    std::env::set_var("SHUNT_TEST_ROUTED_CODEX_B", &token);
    std::env::set_var("SHUNT_TEST_ROUTED_KEY_B", "third-party-key");

    let body = inbound_body("gpt-5.6-sol");
    let codex = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token.clone()))
        .and(header("chatgpt-account-id", "acct-b"))
        .and(body_string(body.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
        .expect(1)
        .mount(&codex)
        .await;
    let third_party = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&third_party)
        .await;

    let gateway = start_gateway_with(test_config(
        &codex.uri(),
        "SHUNT_TEST_ROUTED_CODEX_B",
        &third_party.uri(),
        "SHUNT_TEST_ROUTED_KEY_B",
        vec![route("glm-5.3", "openai", Some("gpt-5.6-sol"))],
    ))
    .await;

    let response = post_responses(&gateway, body).await;

    assert_eq!(response.status(), StatusCode::OK);
    codex.verify().await;
    third_party.verify().await;
}

#[tokio::test]
async fn route_to_a_second_chatgpt_oauth_provider_uses_its_pool() {
    // A route may also name another ChatGPT/Codex provider; that keeps the full
    // pool passthrough (its own accounts, `x-shunt-account`) rather than taking
    // the single-credential third-party path.
    if !can_bind_loopback() {
        return;
    }
    let default_token = chatgpt_token(FAR_FUTURE_EXP, "acct-default");
    let work_token = chatgpt_token(FAR_FUTURE_EXP, "acct-work");
    std::env::set_var("SHUNT_TEST_ROUTED_CODEX_C", &default_token);
    std::env::set_var("SHUNT_TEST_ROUTED_WORK_C", &work_token);
    std::env::set_var("SHUNT_TEST_ROUTED_KEY_C", "unused-key");

    let work = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(work_token.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
        .expect(1)
        .mount(&work)
        .await;
    let default_codex = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&default_codex)
        .await;

    let config = second_chatgpt_config(
        &default_codex.uri(),
        "SHUNT_TEST_ROUTED_CODEX_C",
        "SHUNT_TEST_ROUTED_KEY_C",
        &work.uri(),
        "SHUNT_TEST_ROUTED_WORK_C",
        route("work-model", "codex-work", Some("gpt-5.6-terra")),
    );

    let gateway = start_gateway_with(config).await;
    let response = post_responses(&gateway, inbound_body("work-model")).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("x-shunt-account")
            .and_then(|value| value.to_str().ok()),
        Some("work-account")
    );

    let requests = work.received_requests().await.unwrap();
    let sent: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["model"], "gpt-5.6-terra");
    work.verify().await;
    default_codex.verify().await;
}

#[tokio::test]
async fn a_rewritten_model_drops_the_clients_stale_routing_hint() {
    // `x-codex-routing-hint` names the model the client asked for and is
    // forwarded verbatim by the pool passthrough. Once the body names the
    // route's `upstream_model` instead, that hint points the ChatGPT backend at
    // a model the request no longer asks for, so it must be dropped.
    if !can_bind_loopback() {
        return;
    }
    let work_token = chatgpt_token(FAR_FUTURE_EXP, "acct-hint");
    std::env::set_var(
        "SHUNT_TEST_ROUTED_CODEX_J",
        chatgpt_token(FAR_FUTURE_EXP, "acct-j"),
    );
    std::env::set_var("SHUNT_TEST_ROUTED_WORK_J", &work_token);
    std::env::set_var("SHUNT_TEST_ROUTED_KEY_J", "unused-key");

    let work = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(HeaderAbsent("x-codex-routing-hint"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
        .expect(1)
        .mount(&work)
        .await;

    let gateway = start_gateway_with(second_chatgpt_config(
        "http://127.0.0.1:1",
        "SHUNT_TEST_ROUTED_CODEX_J",
        "SHUNT_TEST_ROUTED_KEY_J",
        &work.uri(),
        "SHUNT_TEST_ROUTED_WORK_J",
        // The rewrite is what invalidates the hint.
        route("work-model", "codex-work", Some("gpt-5.6-terra")),
    ))
    .await;

    let response = post_responses(&gateway, inbound_body("work-model")).await;

    assert_eq!(response.status(), StatusCode::OK);
    let requests = work.received_requests().await.unwrap();
    let sent: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["model"], "gpt-5.6-terra");
    work.verify().await;
}

#[tokio::test]
async fn an_unrewritten_route_still_forwards_the_routing_hint() {
    // The positive twin: the hint is dropped because it went stale, not because
    // routing happened. With `upstream_model == model` it still describes the
    // body, so the pool passthrough forwards it as before.
    if !can_bind_loopback() {
        return;
    }
    let work_token = chatgpt_token(FAR_FUTURE_EXP, "acct-hint-kept");
    std::env::set_var(
        "SHUNT_TEST_ROUTED_CODEX_K",
        chatgpt_token(FAR_FUTURE_EXP, "acct-k"),
    );
    std::env::set_var("SHUNT_TEST_ROUTED_WORK_K", &work_token);
    std::env::set_var("SHUNT_TEST_ROUTED_KEY_K", "unused-key");

    let work = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(header("x-codex-routing-hint", "model=routing-hint-probe"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
        .expect(1)
        .mount(&work)
        .await;

    let gateway = start_gateway_with(second_chatgpt_config(
        "http://127.0.0.1:1",
        "SHUNT_TEST_ROUTED_CODEX_K",
        "SHUNT_TEST_ROUTED_KEY_K",
        &work.uri(),
        "SHUNT_TEST_ROUTED_WORK_K",
        // No `upstream_model`, so nothing about the body changes.
        route("work-model", "codex-work", None),
    ))
    .await;

    let response = post_responses(&gateway, inbound_body("work-model")).await;

    assert_eq!(response.status(), StatusCode::OK);
    work.verify().await;
}

#[tokio::test]
async fn zstd_inbound_body_reaches_a_third_party_identity_encoded() {
    // The Codex CLI zstd-compresses its request body; a stock Responses API does
    // not accept that encoding, so the routed path always decodes it first.
    if !can_bind_loopback() {
        return;
    }
    std::env::set_var(
        "SHUNT_TEST_ROUTED_CODEX_D",
        chatgpt_token(FAR_FUTURE_EXP, "acct-d"),
    );
    std::env::set_var("SHUNT_TEST_ROUTED_KEY_D", "third-party-key");

    let body = inbound_body("glm-5.3");
    let compressed = zstd::stream::encode_all(body.as_bytes(), 3).expect("body should compress");

    let third_party = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(HeaderAbsent("content-encoding"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
        .expect(1)
        .mount(&third_party)
        .await;
    let codex = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&codex)
        .await;

    let gateway = start_gateway_with(test_config(
        &codex.uri(),
        "SHUNT_TEST_ROUTED_CODEX_D",
        &third_party.uri(),
        "SHUNT_TEST_ROUTED_KEY_D",
        vec![route("glm-5.3", "openai", Some("gpt-5.6-sol"))],
    ))
    .await;

    let response = reqwest::Client::new()
        .post(format!("{}/backend-api/codex/responses", gateway.base_url))
        .header("content-type", "application/json")
        .header("content-encoding", "zstd")
        .body(compressed)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let requests = third_party.received_requests().await.unwrap();
    let sent: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("the upstream body should be plain JSON");
    assert_eq!(sent["model"], "gpt-5.6-sol");
    assert_eq!(sent["instructions"], "be brief");
    third_party.verify().await;
    codex.verify().await;
}

#[tokio::test]
async fn third_party_sse_is_relayed_verbatim() {
    if !can_bind_loopback() {
        return;
    }
    std::env::set_var(
        "SHUNT_TEST_ROUTED_CODEX_E",
        chatgpt_token(FAR_FUTURE_EXP, "acct-e"),
    );
    std::env::set_var("SHUNT_TEST_ROUTED_KEY_E", "third-party-key");

    let sse = "event: response.created\ndata: {\"type\":\"response.created\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\"}\n\n";
    let third_party = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .expect(1)
        .mount(&third_party)
        .await;

    let gateway = start_gateway_with(test_config(
        "http://127.0.0.1:1",
        "SHUNT_TEST_ROUTED_CODEX_E",
        &third_party.uri(),
        "SHUNT_TEST_ROUTED_KEY_E",
        vec![route("glm-5.3", "openai", None)],
    ))
    .await;

    let response = post_responses(&gateway, inbound_body("glm-5.3")).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(response.text().await.unwrap(), sse);
    third_party.verify().await;
}

#[tokio::test]
async fn third_party_429_is_relayed_verbatim_without_failover() {
    // There is no pool behind a routed third party, so a 429 is the upstream's
    // own back-off signal: relay it — status, `retry-after`, and body — and send
    // exactly one request.
    if !can_bind_loopback() {
        return;
    }
    std::env::set_var(
        "SHUNT_TEST_ROUTED_CODEX_F",
        chatgpt_token(FAR_FUTURE_EXP, "acct-f"),
    );
    std::env::set_var("SHUNT_TEST_ROUTED_KEY_F", "third-party-key");

    let error_body = r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#;
    let third_party = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "7")
                .set_body_raw(error_body, "application/json"),
        )
        .expect(1)
        .mount(&third_party)
        .await;

    let gateway = start_gateway_with(test_config(
        "http://127.0.0.1:1",
        "SHUNT_TEST_ROUTED_CODEX_F",
        &third_party.uri(),
        "SHUNT_TEST_ROUTED_KEY_F",
        vec![route("glm-5.3", "openai", None)],
    ))
    .await;

    let response = post_responses(&gateway, inbound_body("glm-5.3")).await;

    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("7")
    );
    assert_eq!(response.text().await.unwrap(), error_body);
    third_party.verify().await;
}

#[tokio::test]
async fn inbound_auth_gates_routed_requests() {
    // `[server.auth]` gates the endpoint before any dispatch decision, so an
    // unauthenticated routed request reaches neither upstream — and the 401 is
    // shaped as OpenAI's `{"error":{...}}` envelope, not Anthropic's.
    if !can_bind_loopback() {
        return;
    }
    std::env::set_var(
        "SHUNT_TEST_ROUTED_CODEX_G",
        chatgpt_token(FAR_FUTURE_EXP, "acct-g"),
    );
    std::env::set_var("SHUNT_TEST_ROUTED_KEY_G", "third-party-key");

    let third_party = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&third_party)
        .await;
    let codex = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&codex)
        .await;

    let mut config = test_config(
        &codex.uri(),
        "SHUNT_TEST_ROUTED_CODEX_G",
        &third_party.uri(),
        "SHUNT_TEST_ROUTED_KEY_G",
        vec![route("glm-5.3", "openai", Some("gpt-5.6-sol"))],
    );
    std::env::set_var("SHUNT_TEST_ROUTED_TOKENS_G", "alice:secret-token");
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: "SHUNT_TEST_ROUTED_TOKENS_G".to_string(),
    });

    let gateway = start_gateway_with(config).await;
    let response = reqwest::Client::new()
        .post(format!("{}/backend-api/codex/responses", gateway.base_url))
        .header("content-type", "application/json")
        .body(inbound_body("glm-5.3"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let error: serde_json::Value = response.json().await.unwrap();
    assert_eq!(error["error"]["type"], "authentication_error");
    third_party.verify().await;
    codex.verify().await;
}

#[tokio::test]
async fn unreadable_model_never_matches_a_route() {
    // The `unknown` metrics label is a shunt-authored sentinel; a body whose
    // `model` cannot be read must fall to the fixed pool verbatim rather than
    // match a route (including one declared for the literal model `unknown`).
    if !can_bind_loopback() {
        return;
    }
    let token = chatgpt_token(FAR_FUTURE_EXP, "acct-h");
    std::env::set_var("SHUNT_TEST_ROUTED_CODEX_H", &token);
    std::env::set_var("SHUNT_TEST_ROUTED_KEY_H", "third-party-key");

    let body = r#"{"model":["not","a","string"],"input":[]}"#.to_string();
    let codex = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(BearerToken(token.clone()))
        .and(body_string(body.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
        .expect(1)
        .mount(&codex)
        .await;
    let third_party = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&third_party)
        .await;

    let gateway = start_gateway_with(test_config(
        &codex.uri(),
        "SHUNT_TEST_ROUTED_CODEX_H",
        &third_party.uri(),
        "SHUNT_TEST_ROUTED_KEY_H",
        vec![route("unknown", "openai", Some("gpt-5.6-sol"))],
    ))
    .await;

    let response = post_responses(&gateway, body).await;

    assert_eq!(response.status(), StatusCode::OK);
    codex.verify().await;
    third_party.verify().await;
}

#[tokio::test]
async fn slash_qualified_model_id_routes_and_rewrites() {
    // Vercel AI Gateway and OpenRouter publish provider-qualified slugs
    // (`openai/gpt-5.6-sol`, `~openai/gpt-latest`). The model id is opaque: it
    // must route, rewrite, and label metrics without being rejected or truncated.
    if !can_bind_loopback() {
        return;
    }
    std::env::set_var(
        "SHUNT_TEST_ROUTED_CODEX_I",
        chatgpt_token(FAR_FUTURE_EXP, "acct-i"),
    );
    std::env::set_var("SHUNT_TEST_ROUTED_KEY_I", "third-party-key");

    let third_party = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(BearerToken("third-party-key".to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
        .expect(1)
        .mount(&third_party)
        .await;

    let gateway = start_gateway_with(test_config(
        "http://127.0.0.1:1",
        "SHUNT_TEST_ROUTED_CODEX_I",
        &third_party.uri(),
        "SHUNT_TEST_ROUTED_KEY_I",
        vec![route(
            "openai/gpt-5.6-sol",
            "openai",
            Some("openai/gpt-5.4-mini"),
        )],
    ))
    .await;

    let response = post_responses(&gateway, inbound_body("openai/gpt-5.6-sol")).await;

    assert_eq!(response.status(), StatusCode::OK);
    let requests = third_party.received_requests().await.unwrap();
    let sent: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["model"], "openai/gpt-5.4-mini");
    third_party.verify().await;
}
