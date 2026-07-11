//! Codex WebSocket v2 transport (issue #32) — HTTP fallback safety net.
//!
//! Enabling `websocket = true` must never do worse than plain HTTP: when the
//! websocket cannot be established, the turn is transparently re-driven over the
//! HTTP Responses path. Here the upstream is a plain HTTP mock that has no
//! websocket endpoint, so the handshake fails and the request must still succeed
//! over HTTP.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::PathBuf;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::StatusCode;
use shunt::{
    config::{Config, RouteConfig},
    server,
};
use tokio::task::JoinHandle;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

struct TestGateway {
    base_url: String,
    task: JoinHandle<()>,
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start_gateway_with(mut config: Config) -> TestGateway {
    config.server.bind = "127.0.0.1:0".to_string();
    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _shared) = server::build_router(config).unwrap();
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

/// A minimal unsigned JWT (`x.<payload>.y`) with a far-future `exp`, so the codex
/// auth store treats it as valid without any network refresh.
fn fake_jwt(exp: u64) -> String {
    let payload = serde_json::json!({ "exp": exp });
    format!(
        "x.{}.y",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    )
}

/// Write a codex-style `auth.json` a valid ChatGPT credential can be read from,
/// and point `CODEX_AUTH_FILE` at it. Returns the path for cleanup.
fn write_fake_codex_auth() -> PathBuf {
    let unique_name = format!(
        "shunt-ws-fallback-auth-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let path = std::env::temp_dir().join(unique_name);
    let auth = serde_json::json!({
        "tokens": {
            "access_token": fake_jwt(4_000_000_000),
            "refresh_token": "refresh-xyz",
            "account_id": "acct_fallback"
        }
    });
    std::fs::write(&path, serde_json::to_vec(&auth).unwrap()).unwrap();
    std::env::set_var("CODEX_AUTH_FILE", &path);
    path
}

/// A minimal Responses SSE stream the HTTP path translates into an Anthropic
/// message carrying the assistant text.
const RESPONSES_SSE: &str = concat!(
    "event: response.created\n",
    "data: {\"response\":{\"id\":\"resp_1\",\"usage\":{\"output_tokens\":0}}}\n\n",
    "event: response.output_item.added\n",
    "data: {\"item\":{\"type\":\"message\"}}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"delta\":\"served over HTTP fallback\"}\n\n",
    "event: response.output_text.done\n",
    "data: {}\n\n",
    "event: response.completed\n",
    "data: {\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":4}}}\n\n",
    "data: [DONE]\n\n"
);

#[tokio::test]
async fn websocket_handshake_failure_falls_back_to_http() {
    if !can_bind_loopback() {
        return;
    }

    // Upstream speaks only HTTP: it serves the Responses POST but has no websocket
    // endpoint, so the codex ws handshake (a GET upgrade) 404s and must fall back.
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(RESPONSES_SSE))
        .mount(&upstream)
        .await;

    let auth_path = write_fake_codex_auth();

    let mut config = Config::default();
    {
        let codex = config.providers.get_mut("codex").unwrap();
        codex.base_url = upstream.uri();
        codex.websocket = true; // opt in to the ws transport (should fail → HTTP)
    }
    config.routes.push(RouteConfig {
        model: "codex-fallback-model".to_string(),
        provider: "codex".to_string(),
        upstream_model: None,
        effort: None,
    });

    let gateway = start_gateway_with(config).await;

    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .header("content-type", "application/json")
        .body(
            r#"{"model":"codex-fallback-model","max_tokens":16,"stream":false,"messages":[{"role":"user","content":"hi"}]}"#,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the turn succeeds over the HTTP fallback despite the ws handshake failing"
    );
    let body = response.text().await.unwrap();
    assert!(
        body.contains("served over HTTP fallback"),
        "fallback response carries the upstream's translated text; got: {body}"
    );

    // The upstream saw the HTTP Responses POST (proving the fallback ran).
    let requests = upstream
        .received_requests()
        .await
        .expect("mock records requests");
    assert!(
        requests
            .iter()
            .any(|r| r.method.as_str() == "POST" && r.url.path() == "/codex/responses"),
        "the HTTP Responses endpoint was called by the fallback"
    );

    std::env::remove_var("CODEX_AUTH_FILE");
    let _ = std::fs::remove_file(auth_path);
}
