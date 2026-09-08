use std::{
    io::ErrorKind,
    net::SocketAddr,
    sync::OnceLock,
};

use reqwest::StatusCode;
use serde_json::{json, Value};
use shunt::{config::Config, server};
use tokio::sync::Mutex;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(previous) => std::env::set_var(self.key, previous),
            None => std::env::remove_var(self.key),
        }
    }
}

static OPENAI_CHAT_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

async fn lock_openai_chat_env() -> tokio::sync::MutexGuard<'static, ()> {
    OPENAI_CHAT_ENV_LOCK
        .get_or_init(|| Mutex::const_new(()))
        .lock()
        .await
}

fn can_bind_loopback() -> bool {
    match std::net::TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => {
            drop(listener);
            true
        }
        Err(error) if error.kind() == ErrorKind::PermissionDenied => false,
        Err(error) => panic!("unexpected loopback bind failure: {error}"),
    }
}

/// Builds the fixture config through serde so the RED state fails at the
/// config parser ("unknown variant openai_chat"), not at compile time.
fn config_with_openai_chat_providers(providers: Value, routes: Value) -> Config {
    let mut value = serde_json::to_value(Config::default()).unwrap();
    for (name, provider) in providers.as_object().unwrap() {
        value["providers"][name] = provider.clone();
    }
    value["server"]["default_provider"] = json!(routes.as_array().unwrap()[0]["provider"]);
    value["routes"] = routes;
    serde_json::from_value(value).unwrap()
}

fn openai_chat_provider(base_url: &str, api_key_env: &str) -> Value {
    json!({
        "kind": "openai_chat",
        "base_url": base_url,
        "auth": "api_key",
        "api_key_env": api_key_env
    })
}

fn single_provider_config(base_url: &str) -> Config {
    config_with_openai_chat_providers(
        json!({ "openai-chat-test": openai_chat_provider(base_url, "SHUNT_OPENAI_CHAT_CONFORMANCE_KEY") }),
        json!([{
            "model": "claude-via-chat",
            "provider": "openai-chat-test",
            "upstream_model": "gpt-5",
            "effort": null,
            "service_tier": null
        }]),
    )
}

struct Gateway {
    base_url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_gateway(mut config: Config) -> Gateway {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _, _) = server::build_router(config).unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Gateway {
        base_url: format!("http://{addr}"),
        task,
    }
}

fn anthropic_request(model: &str) -> String {
    json!({
        "model": model,
        "max_tokens": 64,
        "stream": false,
        "messages": [{"role": "user", "content": "fixture"}]
    })
    .to_string()
}

fn anthropic_streaming_request(model: &str) -> String {
    let mut value: Value = serde_json::from_str(&anthropic_request(model)).unwrap();
    value["stream"] = json!(true);
    value.to_string()
}

fn chat_completion_upstream() -> Value {
    json!({
        "id": "chatcmpl-fixture",
        "object": "chat.completion",
        "created": 1,
        "model": "gpt-5",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "fixture reply"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
    })
}

/// Collect the gateway SSE relay into (event, data) frames for assertions.
async fn collect_sse_events(response: reqwest::Response) -> Vec<(String, Value)> {
    use futures_util::StreamExt;
    let mut buffer = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        buffer.extend_from_slice(&chunk.unwrap());
    }
    let text = String::from_utf8(buffer).expect("SSE relay must be UTF-8");
    let mut events = Vec::new();
    for frame in text.split("\n\n") {
        let frame = frame.trim();
        if frame.is_empty() {
            continue;
        }
        let mut event = "message".to_string();
        let mut data = String::new();
        for line in frame.lines() {
            if let Some(rest) = line.strip_prefix("event: ") {
                event = rest.to_string();
            }
            if let Some(rest) = line.strip_prefix("data: ") {
                data.push_str(rest);
            }
        }
        if data.is_empty() {
            continue;
        }
        let value = serde_json::from_str(&data).unwrap_or(Value::Null);
        events.push((event, value));
    }
    events
}

fn sse_body(frames: &[String]) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        // Real upstreams terminate the final frame with a blank line; EOF on
        // an unterminated frame is its own fail-closed case elsewhere.
        .set_body_raw(
            frames
                .iter()
                .map(|frame| format!("data: {frame}\n\n"))
                .collect::<String>(),
            "text/event-stream",
        )
}

fn chat_delta(delta: Value, finish: Option<&str>) -> String {
    let mut choice = json!({ "index": 0, "delta": delta });
    if let Some(finish) = finish {
        choice["finish_reason"] = json!(finish);
    }
    json!({ "choices": [choice] }).to_string()
}

const CHAT_USAGE_CHUNK: &str =
    r#"{"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":2,"total_tokens":9}}"#;

#[tokio::test]
async fn openai_chat_tracer_unary_happy_path() {
    if !can_bind_loopback() {
        return;
    }
    let _env_lock = lock_openai_chat_env().await;
    let _key = EnvVarGuard::set("SHUNT_OPENAI_CHAT_CONFORMANCE_KEY", "fixture-openai-key");

    let backend = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_completion_upstream()))
        .expect(1)
        .mount(&backend)
        .await;

    let gateway = start_gateway(single_provider_config(&backend.uri())).await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("x-api-key", "inbound-slot-key")
        .body(anthropic_request("claude-via-chat"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "fixture reply", "{body}");
    assert_eq!(body["stop_reason"], "end_turn", "{body}");
    assert_eq!(body["usage"]["input_tokens"], 5, "{body}");
    assert_eq!(body["usage"]["output_tokens"], 2, "{body}");

    let requests = backend.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "generation POST must not be re-dispatched");
    // The exact bearer proves the synthetic env key was injected; the absent
    // x-api-key proves the inbound slot was stripped at the wire.
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer fixture-openai-key")
    );
    assert!(
        requests[0].headers.get("x-api-key").is_none(),
        "inbound x-api-key slot must be stripped"
    );
    let upstream_body: Value =
        serde_json::from_slice(&requests[0].body).expect("upstream body is JSON");
    assert_eq!(upstream_body["model"], "gpt-5", "{upstream_body}");
    assert_eq!(upstream_body["stream"], false, "{upstream_body}");
    assert_eq!(upstream_body["messages"][0]["role"], "user", "{upstream_body}");
    assert_eq!(
        upstream_body["messages"][0]["content"], "fixture",
        "{upstream_body}"
    );
}

#[tokio::test]
async fn openai_chat_tracer_unary_isolation() {
    if !can_bind_loopback() {
        return;
    }
    let _env_lock = lock_openai_chat_env().await;
    let _key_a = EnvVarGuard::set("SHUNT_OPENAI_CHAT_CONFORMANCE_KEY_A", "fixture-key-a");
    let _key_b = EnvVarGuard::set("SHUNT_OPENAI_CHAT_CONFORMANCE_KEY_B", "fixture-key-b");

    let backend = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_completion_upstream()))
        .expect(2)
        .mount(&backend)
        .await;

    let config = config_with_openai_chat_providers(
        json!({
            "openai-chat-a": openai_chat_provider(&backend.uri(), "SHUNT_OPENAI_CHAT_CONFORMANCE_KEY_A"),
            "openai-chat-b": openai_chat_provider(&backend.uri(), "SHUNT_OPENAI_CHAT_CONFORMANCE_KEY_B")
        }),
        json!([
            {
                "model": "claude-via-a",
                "provider": "openai-chat-a",
                "upstream_model": "gpt-a",
                "effort": null,
                "service_tier": null
            },
            {
                "model": "claude-via-b",
                "provider": "openai-chat-b",
                "upstream_model": "gpt-b",
                "effort": null,
                "service_tier": null
            }
        ]),
    );
    let gateway = start_gateway(config).await;

    let client = reqwest::Client::new();
    let (response_a, response_b) = tokio::join!(
        client
            .post(format!("{}/v1/messages", gateway.base_url))
            .body(anthropic_request("claude-via-a"))
            .send(),
        client
            .post(format!("{}/v1/messages", gateway.base_url))
            .body(anthropic_request("claude-via-b"))
            .send()
    );
    for response in [response_a.unwrap(), response_b.unwrap()] {
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["content"][0]["text"], "fixture reply", "{body}");
    }

    let requests = backend.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let mut seen = std::collections::HashSet::new();
    for request in &requests {
        let bearer = request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap()
            .to_string();
        let upstream_body: Value =
            serde_json::from_slice(&request.body).expect("upstream body is JSON");
        let model = upstream_body["model"].as_str().unwrap().to_string();
        seen.insert((bearer, model));
    }
    let expected: std::collections::HashSet<(String, String)> = [
        ("Bearer fixture-key-a".to_string(), "gpt-a".to_string()),
        ("Bearer fixture-key-b".to_string(), "gpt-b".to_string()),
    ]
    .into_iter()
    .collect();
    assert_eq!(seen, expected, "credentials and models must stay request-local");
}

#[tokio::test]
async fn openai_chat_tracer_unary_non_api_key_auth_rejected() {
    if !can_bind_loopback() {
        return;
    }
    let _env_lock = lock_openai_chat_env().await;

    let mut value = serde_json::to_value(Config::default()).unwrap();
    value["providers"]["openai-chat-test"] = json!({
        "kind": "openai_chat",
        "base_url": "http://127.0.0.1:9",
        "auth": "passthrough"
    });
    value["server"]["default_provider"] = json!("openai-chat-test");
    value["routes"] = json!([{
        "model": "claude-via-chat",
        "provider": "openai-chat-test",
        "upstream_model": "gpt-5",
        "effort": null,
        "service_tier": null
    }]);
    let config: Config = serde_json::from_value(value).unwrap();
    let error = config.validate().err().expect("non-API-key auth must be rejected");
    let message = error.to_string();
    assert!(message.contains("openai_chat"), "{message}");
    assert!(message.contains("api_key"), "{message}");
}

#[tokio::test]
async fn openai_chat_tracer_streaming_relay() {
    if !can_bind_loopback() {
        return;
    }
    let _env_lock = lock_openai_chat_env().await;
    let _key = EnvVarGuard::set("SHUNT_OPENAI_CHAT_CONFORMANCE_KEY", "fixture-openai-key");

    let backend = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_body(&[
            chat_delta(json!({"role": "assistant"}), None),
            chat_delta(json!({"content": "hello "}), None),
            chat_delta(json!({"content": "world"}), Some("stop")),
            CHAT_USAGE_CHUNK.to_string(),
            "[DONE]".to_string(),
        ]))
        .expect(1)
        .mount(&backend)
        .await;

    let gateway = start_gateway(single_provider_config(&backend.uri())).await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .body(anthropic_streaming_request("claude-via-chat"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let events = collect_sse_events(response).await;

    let text: Vec<&str> = events
        .iter()
        .filter(|(event, _)| event == "content_block_delta")
        .filter_map(|(_, data)| data["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, vec!["hello ", "world"], "{events:?}");
    let stops = events
        .iter()
        .filter(|(event, _)| event == "message_stop")
        .count();
    assert_eq!(stops, 1, "exactly one authoritative terminal: {events:?}");
    assert!(
        events.iter().any(|(event, _)| event == "message_start"),
        "{events:?}"
    );
    let delta = events
        .iter()
        .find(|(event, _)| event == "message_delta")
        .map(|(_, data)| data)
        .expect("message_delta carries the terminal decision");
    assert_eq!(delta["delta"]["stop_reason"], "end_turn", "{delta}");
    assert_eq!(delta["usage"]["input_tokens"], 7, "{delta}");
    assert_eq!(delta["usage"]["output_tokens"], 2, "{delta}");

    let requests = backend.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let upstream_body: Value =
        serde_json::from_slice(&requests[0].body).expect("upstream body is JSON");
    assert_eq!(upstream_body["stream"], true, "{upstream_body}");
    assert_eq!(
        upstream_body["stream_options"]["include_usage"], true,
        "usage accounting must be forced upstream"
    );
}

#[tokio::test]
async fn openai_chat_tracer_streaming_eof_failclosed() {
    if !can_bind_loopback() {
        return;
    }
    let _env_lock = lock_openai_chat_env().await;
    let _key = EnvVarGuard::set("SHUNT_OPENAI_CHAT_CONFORMANCE_KEY", "fixture-openai-key");

    let backend = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_body(&[
            chat_delta(json!({"role": "assistant"}), None),
            chat_delta(json!({"content": "partial"}), None),
        ]))
        .expect(1)
        .mount(&backend)
        .await;

    let gateway = start_gateway(single_provider_config(&backend.uri())).await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .body(anthropic_streaming_request("claude-via-chat"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let events = collect_sse_events(response).await;

    let errors = events
        .iter()
        .filter(|(event, _)| event == "error")
        .count();
    assert_eq!(errors, 1, "EOF without a terminal must fail closed: {events:?}");
    assert!(
        events.iter().all(|(event, _)| event != "message_stop"),
        "a cut stream must never emit a success terminal: {events:?}"
    );
}

#[tokio::test]
async fn openai_chat_tracer_streaming_duplicate_terminal() {
    if !can_bind_loopback() {
        return;
    }
    let _env_lock = lock_openai_chat_env().await;
    let _key = EnvVarGuard::set("SHUNT_OPENAI_CHAT_CONFORMANCE_KEY", "fixture-openai-key");

    let backend = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(sse_body(&[
            chat_delta(json!({"role": "assistant"}), None),
            chat_delta(json!({"content": "done"}), Some("stop")),
            "[DONE]".to_string(),
            "[DONE]".to_string(),
        ]))
        .expect(1)
        .mount(&backend)
        .await;

    let gateway = start_gateway(single_provider_config(&backend.uri())).await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .body(anthropic_streaming_request("claude-via-chat"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let events = collect_sse_events(response).await;

    let errors = events
        .iter()
        .filter(|(event, _)| event == "error")
        .count();
    assert_eq!(
        errors, 1,
        "a duplicate [DONE] terminal must fail closed: {events:?}"
    );
    assert!(
        events.iter().all(|(event, _)| event != "message_stop"),
        "no success terminal may survive a duplicate [DONE]: {events:?}"
    );
}
