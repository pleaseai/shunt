//! chatgpt-plan (`auth = "chatgpt_plan_oauth"`) through the Responses account
//! pool, against a loopback `/v1/responses` and token endpoint.
//!
//! Accounts are real store files under `SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR`, so
//! credential resolution and the 401 refresh run the plan refresh store. The
//! scan orders them by name and round-robin starts at the first, so every
//! first request tries `a` before `b`.

use std::{fs, net::SocketAddr, path::PathBuf};

use reqwest::StatusCode;
use serde_json::{json, Value};
use shunt::{
    accounts::StoreFamily,
    config::{AccountConfig, AuthMode, Config, RouteConfig},
    server,
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{header, method, path},
    Mock, MockServer, ResponseTemplate,
};

mod common;

const PROVIDER: &str = "chatgpt-plan";
const MODEL: &str = "plan-model";
const USAGE_URL: &str = "https://chatgpt.com/settings/usage";

struct Gateway {
    base_url: String,
    state: server::AppState,
    task: JoinHandle<()>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Whether the pool marked scanned store account `name` as needing sign-in.
fn needs_relogin(gateway: &Gateway, name: &str) -> bool {
    let account = AccountConfig {
        name: name.to_string(),
        store_family: Some(StoreFamily::ChatgptPlan),
        store_entry: true,
        ..Default::default()
    };
    gateway.state.accounts.needs_relogin(PROVIDER, &account)
}

fn now() -> std::time::Duration {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "shunt-plan-multi-{tag}-{}-{}",
        std::process::id(),
        now().as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A signed-in account whose access token is a day from expiry, so it is sent
/// as-is until an upstream 401 forces a refresh.
fn write_account(dir: &std::path::Path, name: &str, access: &str, refresh: &str) {
    let record = json!({
        "version": 1,
        "client_id": format!("oaiapp_{name}"),
        "scopes": ["openid", "offline_access", "chatgpt.tokens.use.direct"],
        "tokens": {
            "access_token": access,
            "refresh_token": refresh,
            "expires_at": now().as_secs() + 86_400,
        }
    });
    fs::write(dir.join(format!("{name}.json")), record.to_string()).unwrap();
}

fn plan_config(upstream: &str) -> Config {
    let mut config = Config::default();
    let mut provider = config.providers["codex"].clone();
    provider.auth = AuthMode::ChatgptPlanOauth;
    provider.base_url = format!("{upstream}/v1");
    provider.websocket = false;
    provider.accounts.clear();
    config.providers.insert(PROVIDER.to_string(), provider);
    config.routes.push(RouteConfig {
        model: MODEL.to_string(),
        provider: PROVIDER.to_string(),
        upstream_model: Some("gpt-6.1-sol".to_string()),
        effort: None,
        service_tier: None,
    });
    config
}

async fn start(upstream: &MockServer) -> Gateway {
    let mut config = plan_config(&upstream.uri());
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared, state) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Gateway {
        base_url: format!("http://{addr}"),
        state,
        task,
    }
}

async fn post(gateway: &Gateway, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("x-claude-code-session-id", "session-plan")
        .json(&body)
        .send()
        .await
        .unwrap()
}

fn simple_turn(stream: bool) -> Value {
    json!({
        "model": MODEL,
        "max_tokens": 64,
        "stream": stream,
        "messages": [{"role": "user", "content": "hi"}],
    })
}

fn text_sse(text: &str) -> String {
    format!(
        "event: response.created\ndata: {{\"response\":{{\"id\":\"resp_1\"}}}}\n\n\
         event: response.output_item.added\ndata: {{\"item\":{{\"type\":\"message\"}}}}\n\n\
         event: response.output_text.delta\ndata: {{\"delta\":\"{text}\"}}\n\n\
         event: response.output_text.done\ndata: {{}}\n\n\
         event: response.completed\ndata: {{\"response\":{{\"usage\":{{\"input_tokens\":5,\"output_tokens\":2}}}}}}\n\n"
    )
}

const TOOL_SSE: &str = concat!(
    "event: response.created\ndata: {\"response\":{\"id\":\"resp_t\"}}\n\n",
    "event: response.output_item.added\n",
    "data: {\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read_file\"}}\n\n",
    "event: response.function_call_arguments.delta\n",
    "data: {\"delta\":\"{\\\"path\\\":\\\"Cargo.toml\\\"}\"}\n\n",
    "event: response.function_call_arguments.done\n",
    "data: {\"arguments\":\"{\\\"path\\\":\\\"Cargo.toml\\\"}\"}\n\n",
    "event: response.completed\n",
    "data: {\"response\":{\"usage\":{\"input_tokens\":9,\"output_tokens\":4}}}\n\n",
);

fn sse_ok(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body.to_string())
}

fn siwc(status: u16, code: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(json!({
        "error": {"message": format!("{code} from upstream"), "code": code}
    }))
}

async fn mount(server: &MockServer, token: &str, response: ResponseTemplate, times: u64) {
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .respond_with(response)
        .expect(times)
        .mount(server)
        .await;
}

/// A token endpoint that must see exactly `times` refresh POSTs.
async fn token_endpoint(vars: &mut common::EnvVars, fresh: &str, times: u64) -> MockServer {
    let auth = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": fresh,
            "refresh_token": "refresh-rotated",
            "expires_in": 3600,
        })))
        .expect(times)
        .mount(&auth)
        .await;
    vars.set(
        "SHUNT_CHATGPT_PLAN_TOKEN_URL",
        format!("{}/token", auth.uri()),
    );
    auth
}

async fn error_body(response: reqwest::Response) -> Value {
    serde_json::from_str(&response.text().await.unwrap()).unwrap()
}

#[tokio::test]
async fn tools_and_thinking_reach_upstream_as_plain_functions_with_a_plan_bearer() {
    let mut vars = common::env_lock().await;
    let dir = temp_dir("tools");
    write_account(&dir, "a", "plan-a", "refresh-a");
    vars.set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &dir);
    let upstream = MockServer::start().await;
    mount(&upstream, "plan-a", sse_ok(TOOL_SSE), 1).await;
    let gateway = start(&upstream).await;

    let response = post(
        &gateway,
        json!({
            "model": MODEL,
            "max_tokens": 512,
            "stream": true,
            "temperature": 0.2,
            "metadata": {"user_id": "u-1"},
            "thinking": {"type": "enabled", "budget_tokens": 1024},
            "tools": [{
                "name": "read_file",
                "description": "Read a file",
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}
            }],
            "messages": [{"role": "user", "content": "read Cargo.toml"}],
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let sse = response.text().await.unwrap();
    assert!(sse.contains(r#""type":"tool_use""#), "{sse}");
    assert!(sse.contains(r#""name":"read_file""#), "{sse}");
    assert!(sse.contains(r#""stop_reason":"tool_use""#), "{sse}");

    let sent = &upstream.received_requests().await.unwrap()[0];
    let headers = &sent.headers;
    assert_eq!(headers["authorization"], "Bearer plan-a");
    for absent in ["chatgpt-account-id", "originator", "version"] {
        assert!(headers.get(absent).is_none(), "{absent} must not be sent");
    }
    assert_eq!(headers["session-id"], "session-plan");
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    let tool = &body["tools"][0];
    assert_eq!(tool["type"], "function", "{body}");
    assert_eq!(tool["name"], "read_file", "{body}");
    assert_eq!(body["max_output_tokens"], 512, "{body}");
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert!(body.get("reasoning").is_some(), "{body}");
    for absent in ["temperature", "metadata", "prompt_cache_retention"] {
        assert!(body.get(absent).is_none(), "{absent} leaked: {body}");
    }
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_non_streaming_client_is_served_from_a_streamed_upstream() {
    let mut vars = common::env_lock().await;
    let dir = temp_dir("json");
    write_account(&dir, "a", "plan-a", "refresh-a");
    vars.set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &dir);
    let upstream = MockServer::start().await;
    mount(&upstream, "plan-a", sse_ok(&text_sse("hello plan")), 1).await;
    let gateway = start(&upstream).await;

    let response = post(&gateway, simple_turn(false)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let message: Value = response.json().await.unwrap();
    assert_eq!(message["type"], "message", "{message}");
    assert_eq!(message["content"][0]["text"], "hello plan", "{message}");
    assert_eq!(message["stop_reason"], "end_turn", "{message}");

    let sent = &upstream.received_requests().await.unwrap()[0];
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(body["stream"], true, "{body}");
    assert_eq!(body["store"], false, "{body}");
    assert_eq!(body["max_output_tokens"], 64, "{body}");
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn http_429_rotates_to_the_next_account_in_both_loops() {
    let mut vars = common::env_lock().await;
    let dir = temp_dir("429");
    write_account(&dir, "a", "plan-a", "refresh-a");
    write_account(&dir, "b", "plan-b", "refresh-b");
    vars.set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &dir);
    let upstream = MockServer::start().await;
    let limit = siwc(429, "subscription_sharing_usage_limit_exceeded");
    mount(&upstream, "plan-a", limit, 1).await;
    mount(&upstream, "plan-b", sse_ok(&text_sse("from b")), 2).await;
    let gateway = start(&upstream).await;

    let response = post(&gateway, simple_turn(false)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-shunt-account"], "b");
    assert!(response.text().await.unwrap().contains("from b"));

    // `a` is cooling now, so the streaming loop goes straight to `b`.
    let response = post(&gateway, simple_turn(true)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.text().await.unwrap().contains("from b"));
    upstream.verify().await;
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn http_401_refreshes_once_and_retries_the_same_account() {
    let mut vars = common::env_lock().await;
    let dir = temp_dir("401");
    write_account(&dir, "a", "plan-stale", "refresh-a");
    vars.set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &dir);
    let auth = token_endpoint(&mut vars, "plan-fresh", 1).await;
    let upstream = MockServer::start().await;
    let invalid = siwc(401, "subscription_sharing_invalid_user");
    mount(&upstream, "plan-stale", invalid, 1).await;
    mount(
        &upstream,
        "plan-fresh",
        sse_ok(&text_sse("after refresh")),
        1,
    )
    .await;
    let gateway = start(&upstream).await;

    let response = post(&gateway, simple_turn(false)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.text().await.unwrap().contains("after refresh"));
    let stored: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("a.json")).unwrap()).unwrap();
    assert_eq!(stored["tokens"]["refresh_token"], "refresh-rotated");
    assert!(!needs_relogin(&gateway, "a"));
    upstream.verify().await;
    auth.verify().await;
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_401_after_the_refresh_marks_the_account_for_sign_in() {
    let mut vars = common::env_lock().await;
    let dir = temp_dir("401-twice");
    write_account(&dir, "a", "plan-stale", "refresh-a");
    vars.set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &dir);
    let auth = token_endpoint(&mut vars, "plan-fresh", 1).await;
    let upstream = MockServer::start().await;
    for token in ["plan-stale", "plan-fresh"] {
        mount(
            &upstream,
            token,
            siwc(401, "subscription_sharing_invalid_user"),
            1,
        )
        .await;
    }
    let gateway = start(&upstream).await;

    let response = post(&gateway, simple_turn(false)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = error_body(response).await;
    assert_eq!(body["error"]["type"], "authentication_error", "{body}");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("shunt login chatgpt-plan --name"),
        "{message}"
    );
    assert!(needs_relogin(&gateway, "a"));
    upstream.verify().await;
    auth.verify().await;
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn permission_403_codes_relay_without_a_retry_or_a_refresh() {
    for code in [
        "subscription_sharing_user_not_eligible",
        "subscription_sharing_route_not_supported",
        "chatpass_v2_scope_not_authorized",
    ] {
        let mut vars = common::env_lock().await;
        let dir = temp_dir("403");
        write_account(&dir, "a", "plan-a", "refresh-a");
        write_account(&dir, "b", "plan-b", "refresh-b");
        vars.set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &dir);
        let auth = token_endpoint(&mut vars, "never-issued", 0).await;
        let upstream = MockServer::start().await;
        mount(&upstream, "plan-a", siwc(403, code), 1).await;
        mount(&upstream, "plan-b", sse_ok(&text_sse("b")), 0).await;
        let gateway = start(&upstream).await;

        let response = post(&gateway, simple_turn(false)).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{code}");
        let body = error_body(response).await;
        assert_eq!(body["error"]["type"], "permission_error", "{code}: {body}");
        assert!(!needs_relogin(&gateway, "a"));
        upstream.verify().await;
        auth.verify().await;
        fs::remove_dir_all(&dir).ok();
    }
}

#[tokio::test]
async fn unavailable_503_rotates_keeps_credentials_and_exhausts_as_overloaded() {
    let mut vars = common::env_lock().await;
    let dir = temp_dir("503");
    write_account(&dir, "a", "plan-a", "refresh-a");
    write_account(&dir, "b", "plan-b", "refresh-b");
    let before = [
        fs::read(dir.join("a.json")).unwrap(),
        fs::read(dir.join("b.json")).unwrap(),
    ];
    vars.set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &dir);
    let auth = token_endpoint(&mut vars, "never-issued", 0).await;
    let upstream = MockServer::start().await;
    mount(
        &upstream,
        "plan-a",
        siwc(503, "subscription_sharing_usage_unavailable"),
        1,
    )
    .await;
    mount(
        &upstream,
        "plan-b",
        siwc(503, "subscription_sharing_user_unavailable"),
        1,
    )
    .await;
    let gateway = start(&upstream).await;

    let response = post(&gateway, simple_turn(false)).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = error_body(response).await;
    assert_eq!(body["error"]["type"], "overloaded_error", "{body}");
    let after = [
        fs::read(dir.join("a.json")).unwrap(),
        fs::read(dir.join("b.json")).unwrap(),
    ];
    assert_eq!(before, after, "credentials must be kept");
    for name in ["a", "b"] {
        assert!(!needs_relogin(&gateway, name));
    }
    upstream.verify().await;
    auth.verify().await;
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn every_account_on_its_usage_limit_points_to_chatgpt_usage_settings() {
    let mut vars = common::env_lock().await;
    let dir = temp_dir("all-429");
    write_account(&dir, "a", "plan-a", "refresh-a");
    write_account(&dir, "b", "plan-b", "refresh-b");
    vars.set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &dir);
    let upstream = MockServer::start().await;
    for token in ["plan-a", "plan-b"] {
        let limit = siwc(429, "subscription_sharing_usage_limit_exceeded");
        mount(&upstream, token, limit, 2).await;
    }
    let gateway = start(&upstream).await;

    let response = post(&gateway, simple_turn(false)).await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = error_body(response).await;
    assert_eq!(body["error"]["type"], "rate_limit_error", "{body}");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(message.contains(USAGE_URL), "{message}");

    // Both accounts are cooling now; a streaming turn gets the same error.
    let response = post(&gateway, simple_turn(true)).await;
    let sse = response.text().await.unwrap();
    assert!(sse.contains("rate_limit_error"), "{sse}");
    assert!(sse.contains(USAGE_URL), "{sse}");
    upstream.verify().await;
    fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn an_empty_plan_pool_names_the_sign_in_command() {
    let mut vars = common::env_lock().await;
    let dir = temp_dir("empty");
    vars.set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &dir);
    let upstream = MockServer::start().await;
    let gateway = start(&upstream).await;

    let response = post(&gateway, simple_turn(false)).await;
    assert!(!response.status().is_success());
    let text = response.text().await.unwrap();
    assert!(text.contains("shunt login chatgpt-plan --name"), "{text}");
    assert!(upstream.received_requests().await.unwrap().is_empty());
    fs::remove_dir_all(&dir).ok();
}
