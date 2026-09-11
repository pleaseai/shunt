//! End-to-end coverage for the opt-in inbound Responses WebSocket transport.

use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::{SinkExt, StreamExt};
use shunt::{
    config::{AccountConfig, CodexEndpointConfig, CodexRouteConfig, Config, InboundAuthConfig},
    reload, server,
};
use tokio::sync::{mpsc, Notify};
use tokio_tungstenite::{connect_async, tungstenite::Message, WebSocketStream};
use tungstenite::client::IntoClientRequest;

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct RunningServer {
    address: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone)]
struct UpstreamState {
    replies: Arc<Mutex<VecDeque<Reply>>>,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

enum Reply {
    Static {
        status: StatusCode,
        content_type: &'static str,
        body: String,
        headers: Vec<(&'static str, &'static str)>,
    },
    StaticBytes {
        status: StatusCode,
        content_type: &'static str,
        body: Vec<u8>,
    },
    Held {
        initial: String,
        started: Arc<Notify>,
        dropped: Arc<Notify>,
    },
}

async fn upstream_response(State(state): State<UpstreamState>, body: Bytes) -> Response {
    state
        .requests
        .lock()
        .unwrap()
        .push(serde_json::from_slice(&body).unwrap());
    let reply = state.replies.lock().unwrap().pop_front().unwrap();
    match reply {
        Reply::Static {
            status,
            content_type,
            body,
            headers,
        } => {
            let mut response = (status, body).into_response();
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
            for (name, value) in headers {
                response.headers_mut().insert(
                    axum::http::HeaderName::from_static(name),
                    HeaderValue::from_static(value),
                );
            }
            response
        }
        Reply::StaticBytes {
            status,
            content_type,
            body,
        } => {
            let mut response = (status, body).into_response();
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
            response
        }
        Reply::Held {
            initial,
            started,
            dropped,
        } => {
            let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(1);
            tx.send(Ok(Bytes::from(initial))).await.unwrap();
            started.notify_one();
            tokio::spawn(async move {
                tx.closed().await;
                dropped.notify_one();
            });
            let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            });
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                Body::from_stream(stream),
            )
                .into_response()
        }
    }
}

async fn start_server(router: Router) -> RunningServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    RunningServer { address, task }
}

async fn start_upstream(replies: Vec<Reply>) -> (RunningServer, UpstreamState) {
    let state = UpstreamState {
        replies: Arc::new(Mutex::new(replies.into())),
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    let router = Router::new()
        .route("/codex/responses", post(upstream_response))
        .route("/responses", post(upstream_response))
        .with_state(state.clone());
    (start_server(router).await, state)
}

async fn start_native_gateway(
    upstream: &RunningServer,
    suffix: &str,
) -> (RunningServer, String, String, String, EnvCleanup) {
    let account_env = format!("SHUNT_TEST_INBOUND_WS_ACCOUNT_{suffix}");
    let client_env = format!("SHUNT_TEST_INBOUND_WS_CLIENT_{suffix}");
    let api_env = format!("SHUNT_TEST_INBOUND_WS_API_{suffix}");
    std::env::set_var(&account_env, access_token("account-1"));
    std::env::set_var(&client_env, "client:gateway-secret");
    std::env::set_var(&api_env, "native-api-key");

    let mut config = Config::default();
    let codex = config.providers.get_mut("codex").unwrap();
    codex.base_url = format!("http://{}", upstream.address);
    codex.accounts = vec![AccountConfig {
        name: "account-1".to_string(),
        token_env: Some(account_env.clone()),
        ..Default::default()
    }];
    let openai = config.providers.get_mut("openai").unwrap();
    openai.base_url = format!("http://{}", upstream.address);
    openai.api_key_env = Some(api_env.clone());
    config.server.codex_endpoint = Some(CodexEndpointConfig {
        provider: "codex".to_string(),
        routes: vec![CodexRouteConfig {
            model: "native-ws-model".to_string(),
            provider: "openai".to_string(),
            upstream_model: None,
        }],
    });
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: client_env.clone(),
    });
    let (router, _, _) = server::build_router(config).unwrap();
    let cleanup = EnvCleanup::new([account_env.clone(), client_env.clone(), api_env.clone()]);
    (
        start_server(router).await,
        account_env,
        client_env,
        api_env,
        cleanup,
    )
}

fn access_token(account_id: &str) -> String {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let payload = serde_json::json!({
        "exp": exp,
        "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
    });
    format!(
        "x.{}.y",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    )
}

async fn start_gateway(
    upstream: &RunningServer,
    suffix: &str,
) -> (RunningServer, String, String, EnvCleanup) {
    let account_env = format!("SHUNT_TEST_INBOUND_WS_ACCOUNT_{suffix}");
    let client_env = format!("SHUNT_TEST_INBOUND_WS_CLIENT_{suffix}");
    std::env::set_var(&account_env, access_token("account-1"));
    std::env::set_var(&client_env, "client:gateway-secret");

    let mut config = Config::default();
    let provider = config.providers.get_mut("codex").unwrap();
    provider.base_url = format!("http://{}", upstream.address);
    provider.accounts = vec![AccountConfig {
        name: "account-1".to_string(),
        token_env: Some(account_env.clone()),
        ..Default::default()
    }];
    config.server.codex_endpoint = Some(CodexEndpointConfig {
        provider: "codex".to_string(),
        routes: Vec::new(),
    });
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: client_env.clone(),
    });
    let (router, _, _) = server::build_router(config).unwrap();
    let cleanup = EnvCleanup::new([account_env.clone(), client_env.clone()]);
    (start_server(router).await, account_env, client_env, cleanup)
}

async fn connect(
    gateway: &RunningServer,
    path: &str,
) -> WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let mut request = format!("ws://{}{path}", gateway.address)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", "Bearer gateway-secret".parse().unwrap());
    connect_async(request).await.unwrap().0
}

async fn send_create(
    socket: &mut WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    marker: &str,
) {
    send_create_model(socket, "gpt-5.4-mini", marker).await;
}

async fn send_create_model(
    socket: &mut WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    model: &str,
    marker: &str,
) {
    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "response.create",
                "generate": true,
                "model": model,
                "stream": false,
                "metadata": {"marker": marker},
                "input": []
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
}

async fn next_json(
    socket: &mut WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
) -> serde_json::Value {
    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("timed out waiting for websocket frame")
        .expect("socket ended")
        .expect("websocket read failed");
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}

struct EnvCleanup {
    names: Vec<String>,
}

impl EnvCleanup {
    fn new(names: impl IntoIterator<Item = String>) -> Self {
        Self {
            names: names.into_iter().collect(),
        }
    }
}

impl Drop for EnvCleanup {
    fn drop(&mut self) {
        for name in &self.names {
            std::env::remove_var(name);
        }
    }
}

fn cleanup(account_env: &str, client_env: &str) {
    std::env::remove_var(account_env);
    std::env::remove_var(client_env);
}

#[tokio::test]
async fn disabled_and_unauthorized_upgrades_fail_before_websocket_open() {
    let _env = ENV_LOCK.lock().await;
    let (disabled_router, _, _) = server::build_router(Config::default()).unwrap();
    let disabled = start_server(disabled_router).await;
    let (upstream, _) = start_upstream(Vec::new()).await;
    let (gateway, account_env, client_env, _env_cleanup) = start_gateway(&upstream, "AUTH").await;
    let client = reqwest::Client::new();

    for path in [
        "/backend-api/codex/responses",
        "/responses",
        "/v1/responses",
    ] {
        let disabled_response = client
            .get(format!("http://{}{path}", disabled.address))
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .send()
            .await
            .unwrap();
        assert_eq!(disabled_response.status(), StatusCode::NOT_FOUND);

        for authorization in [None, Some("Bearer wrong-secret")] {
            let mut request = client
                .get(format!("http://{}{path}", gateway.address))
                .header("connection", "upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==");
            if let Some(value) = authorization {
                request = request.header("authorization", value);
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let error: serde_json::Value = response.json().await.unwrap();
            assert_eq!(error["error"]["type"], "authentication_error");
        }
    }
    cleanup(&account_env, &client_env);
}

#[tokio::test]
async fn warmup_is_local_and_all_registered_paths_upgrade() {
    let _env = ENV_LOCK.lock().await;
    let (upstream, state) = start_upstream(Vec::new()).await;
    let (gateway, account_env, client_env, _env_cleanup) = start_gateway(&upstream, "PATHS").await;

    for path in [
        "/backend-api/codex/responses",
        "/responses",
        "/v1/responses",
    ] {
        let mut socket = connect(&gateway, path).await;
        socket
            .send(Message::Text(
                r#"{"type":"response.create","generate":false,"model":"gpt-5.4-mini"}"#.into(),
            ))
            .await
            .unwrap();
        let created = next_json(&mut socket).await;
        let completed = next_json(&mut socket).await;
        assert_eq!(created["type"], "response.created");
        assert_eq!(created["response"]["id"], "");
        assert_eq!(completed["type"], "response.completed");
        assert_eq!(completed["response"]["id"], "");
    }
    assert!(state.requests.lock().unwrap().is_empty());
    cleanup(&account_env, &client_env);
}

#[tokio::test]
async fn streams_ordered_payloads_and_forces_streaming_upstream() {
    let _env = ENV_LOCK.lock().await;
    let body = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"stale\"}\n\n"
    );
    let (upstream, state) = start_upstream(vec![Reply::Static {
        status: StatusCode::OK,
        content_type: "text/event-stream",
        body: body.to_string(),
        headers: Vec::new(),
    }])
    .await;
    let (gateway, account_env, client_env, _env_cleanup) = start_gateway(&upstream, "STREAM").await;
    let mut socket = connect(&gateway, "/v1/responses").await;
    send_create(&mut socket, "live").await;

    assert_eq!(next_json(&mut socket).await["type"], "response.created");
    assert_eq!(next_json(&mut socket).await["delta"], "hi");
    assert_eq!(next_json(&mut socket).await["type"], "response.completed");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), socket.next())
            .await
            .is_err()
    );
    let requests = state.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["stream"], true);
    assert!(requests[0].get("type").is_none());
    assert!(requests[0].get("generate").is_none());
    cleanup(&account_env, &client_env);
}

#[tokio::test]
async fn omitted_generate_defaults_to_a_live_turn() {
    let _env = ENV_LOCK.lock().await;
    let (upstream, state) = start_upstream(vec![Reply::Static {
        status: StatusCode::OK,
        content_type: "text/event-stream",
        body: "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r-default\"}}\n\n"
            .to_string(),
        headers: Vec::new(),
    }])
    .await;
    let (gateway, account_env, client_env, _env_cleanup) =
        start_gateway(&upstream, "DEFAULT_GENERATE").await;
    let mut socket = connect(&gateway, "/v1/responses").await;

    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "response.create",
                "model": "gpt-5.4-mini",
                "stream": false,
                "input": []
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();

    assert_eq!(next_json(&mut socket).await["type"], "response.completed");
    let requests = state.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["stream"], true);
    assert!(requests[0].get("type").is_none());
    assert!(requests[0].get("generate").is_none());
    cleanup(&account_env, &client_env);
}

#[tokio::test]
async fn native_route_matches_http_and_websocket_provider_selection() {
    let _env = ENV_LOCK.lock().await;
    let body = "data: {\"type\":\"response.completed\",\"marker\":\"native\"}\n\n";
    let (upstream, state) = start_upstream(vec![
        Reply::Static {
            status: StatusCode::OK,
            content_type: "text/event-stream",
            body: body.to_string(),
            headers: Vec::new(),
        },
        Reply::Static {
            status: StatusCode::OK,
            content_type: "text/event-stream",
            body: body.to_string(),
            headers: Vec::new(),
        },
    ])
    .await;
    let (gateway, account_env, client_env, api_env, _env_cleanup) =
        start_native_gateway(&upstream, "NATIVE_PARITY").await;

    let http = reqwest::Client::new()
        .post(format!("http://{}/v1/responses", gateway.address))
        .header("x-shunt-token", "gateway-secret")
        .json(&serde_json::json!({
            "model": "native-ws-model",
            "stream": true,
            "input": []
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(http.status(), StatusCode::OK);
    assert!(http.text().await.unwrap().contains("response.completed"));

    let mut socket = connect(&gateway, "/v1/responses").await;
    send_create_model(&mut socket, "native-ws-model", "ws").await;
    assert_eq!(next_json(&mut socket).await["type"], "response.completed");

    let requests = state.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests
        .iter()
        .all(|request| request["model"] == "native-ws-model"));
    cleanup(&account_env, &client_env);
    std::env::remove_var(api_env);
}

#[tokio::test]
async fn missing_model_websocket_uses_pinned_fallback_even_when_unknown_route_exists() {
    let _env = ENV_LOCK.lock().await;
    let body_pinned = "data: {\"type\":\"response.completed\",\"marker\":\"pinned-codex\"}\n\n";
    let (upstream_pinned, state_pinned) = start_upstream(vec![
        Reply::Static {
            status: StatusCode::OK,
            content_type: "text/event-stream",
            body: body_pinned.to_string(),
            headers: Vec::new(),
        },
        Reply::Static {
            status: StatusCode::OK,
            content_type: "text/event-stream",
            body: body_pinned.to_string(),
            headers: Vec::new(),
        },
    ])
    .await;
    let body_unknown = "data: {\"type\":\"response.completed\",\"marker\":\"unknown-route\"}\n\n";
    let (upstream_unknown, state_unknown) = start_upstream(vec![Reply::Static {
        status: StatusCode::OK,
        content_type: "text/event-stream",
        body: body_unknown.to_string(),
        headers: Vec::new(),
    }])
    .await;

    let account_env = "SHUNT_TEST_INBOUND_WS_ACCOUNT_UNKNOWN_ROUTE";
    let client_env = "SHUNT_TEST_INBOUND_WS_CLIENT_UNKNOWN_ROUTE";
    let api_env = "SHUNT_TEST_INBOUND_WS_API_UNKNOWN_ROUTE";
    std::env::set_var(account_env, access_token("account-1"));
    std::env::set_var(client_env, "client:gateway-secret");
    std::env::set_var(api_env, "native-api-key");
    let _env_cleanup = EnvCleanup::new([
        account_env.to_string(),
        client_env.to_string(),
        api_env.to_string(),
    ]);

    let mut config = Config::default();
    let codex = config.providers.get_mut("codex").unwrap();
    codex.base_url = format!("http://{}", upstream_pinned.address);
    codex.accounts = vec![AccountConfig {
        name: "account-1".to_string(),
        token_env: Some(account_env.to_string()),
        ..Default::default()
    }];
    let openai = config.providers.get_mut("openai").unwrap();
    openai.base_url = format!("http://{}", upstream_unknown.address);
    openai.api_key_env = Some(api_env.to_string());
    config.server.codex_endpoint = Some(CodexEndpointConfig {
        provider: "codex".to_string(),
        routes: vec![CodexRouteConfig {
            model: "unknown".to_string(),
            provider: "openai".to_string(),
            upstream_model: None,
        }],
    });
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: client_env.to_string(),
    });
    let (router, _, _) = server::build_router(config).unwrap();
    let gateway = start_server(router).await;

    // HTTP request without model uses pinned fallback (codex), not the "unknown" route.
    let http = reqwest::Client::new()
        .post(format!("http://{}/v1/responses", gateway.address))
        .header("x-shunt-token", "gateway-secret")
        .json(&serde_json::json!({
            "stream": true,
            "input": []
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(http.status(), StatusCode::OK);
    assert!(http.text().await.unwrap().contains("pinned-codex"));

    // WebSocket request without model also uses pinned fallback (codex), matching HTTP parity.
    let mut socket = connect(&gateway, "/v1/responses").await;
    socket
        .send(Message::Text(
            serde_json::json!({
                "type": "response.create",
                "stream": false,
                "input": []
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let frame = next_json(&mut socket).await;
    assert_eq!(frame["marker"], "pinned-codex");

    assert_eq!(state_pinned.requests.lock().unwrap().len(), 2);
    assert!(state_unknown.requests.lock().unwrap().is_empty());
    cleanup(account_env, client_env);
    std::env::remove_var(api_env);
}

#[tokio::test]
async fn hot_reload_snapshot_routes_each_websocket_turn_once() {
    let _env = ENV_LOCK.lock().await;
    let reply = |marker: &'static str| Reply::Static {
        status: StatusCode::OK,
        content_type: "text/event-stream",
        body: format!("data: {{\"type\":\"response.completed\",\"marker\":\"{marker}\"}}\n\n"),
        headers: Vec::new(),
    };
    let (upstream_a, state_a) = start_upstream(vec![reply("A")]).await;
    let (upstream_b, state_b) = start_upstream(vec![reply("B")]).await;

    let account_env = "SHUNT_TEST_INBOUND_WS_ACCOUNT_RELOAD";
    let client_env = "SHUNT_TEST_INBOUND_WS_CLIENT_RELOAD";
    let api_env = "SHUNT_TEST_INBOUND_WS_API_RELOAD";
    std::env::set_var(account_env, access_token("account-1"));
    std::env::set_var(client_env, "client:gateway-secret");
    std::env::set_var(api_env, "native-api-key");

    let mut config = Config::default();
    config.providers.get_mut("codex").unwrap().base_url = format!("http://{}", upstream_a.address);
    let openai = config.providers.get_mut("openai").unwrap();
    openai.base_url = format!("http://{}", upstream_a.address);
    openai.api_key_env = Some(api_env.to_string());
    config.server.codex_endpoint = Some(CodexEndpointConfig {
        provider: "codex".to_string(),
        routes: vec![CodexRouteConfig {
            model: "reload-ws-model".to_string(),
            provider: "openai".to_string(),
            upstream_model: None,
        }],
    });
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: client_env.to_string(),
    });
    let (router, shared, _) = server::build_router(config.clone()).unwrap();
    let gateway = start_server(router).await;
    let mut socket = connect(&gateway, "/v1/responses").await;

    send_create_model(&mut socket, "reload-ws-model", "first").await;
    assert_eq!(next_json(&mut socket).await["marker"], "A");
    assert_eq!(state_a.requests.lock().unwrap().len(), 1);
    assert!(state_b.requests.lock().unwrap().is_empty());

    config.providers.get_mut("openai").unwrap().base_url = format!("http://{}", upstream_b.address);
    let dir = std::env::temp_dir().join(format!(
        "shunt-inbound-ws-reload-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("shunt.toml");
    let mut reloaded_toml = toml::to_string(&config).unwrap();
    // Empty `upstreams` serializes as an explicit declaration, which conflicts
    // with the legacy `[providers.*]` form when Config::load validates reloads.
    reloaded_toml = reloaded_toml.replace("upstreams = []\n", "");
    std::fs::write(&path, reloaded_toml).unwrap();
    reload::reload(&shared, Some(&path)).expect("reload native route config");

    send_create_model(&mut socket, "reload-ws-model", "second").await;
    assert_eq!(next_json(&mut socket).await["marker"], "B");
    assert_eq!(state_a.requests.lock().unwrap().len(), 1);
    assert_eq!(state_b.requests.lock().unwrap().len(), 1);

    std::fs::remove_dir_all(dir).ok();
    std::env::remove_var(account_env);
    std::env::remove_var(client_env);
    std::env::remove_var(api_env);
}

#[tokio::test]
async fn no_mid_stream_hop() {
    let _env = ENV_LOCK.lock().await;
    let (upstream, state) = start_upstream(vec![Reply::Static {
        status: StatusCode::OK,
        content_type: "text/event-stream",
        body:
            "data: {\"type\":\"response.created\"}\n\ndata: {\"type\":\"response.completed\"}\n\n"
                .to_string(),
        headers: Vec::new(),
    }])
    .await;
    let (gateway, account_env, client_env, _env_cleanup) = start_gateway(&upstream, "NO_HOP").await;
    let mut socket = connect(&gateway, "/v1/responses").await;
    send_create(&mut socket, "no-hop").await;
    assert_eq!(next_json(&mut socket).await["type"], "response.created");
    assert_eq!(next_json(&mut socket).await["type"], "response.completed");
    assert_eq!(state.requests.lock().unwrap().len(), 1);
    cleanup(&account_env, &client_env);
}

#[tokio::test]
async fn responses_terminal_premature_done_and_malformed_sse_become_protocol_errors() {
    let _env = ENV_LOCK.lock().await;
    let replies = vec![
        Reply::Static {
            status: StatusCode::OK,
            content_type: "text/event-stream",
            body: "data: [DONE]\n\n".to_string(),
            headers: Vec::new(),
        },
        Reply::Static {
            status: StatusCode::OK,
            content_type: "text/event-stream",
            body: "data: not-json\n\n".to_string(),
            headers: Vec::new(),
        },
    ];
    let (upstream, _) = start_upstream(replies).await;
    let (gateway, account_env, client_env, _env_cleanup) = start_gateway(&upstream, "ERRORS").await;
    let mut socket = connect(&gateway, "/v1/responses").await;

    send_create(&mut socket, "done").await;
    let done_error = next_json(&mut socket).await;
    assert_eq!(done_error["status"], 502);
    assert_eq!(done_error["error"]["code"], "websocket_protocol_error");
    assert_eq!(
        done_error["error"]["message"],
        "Upstream stream ended before response terminal event"
    );

    send_create(&mut socket, "malformed").await;
    let malformed = next_json(&mut socket).await;
    assert_eq!(malformed["status"], 502);
    assert_eq!(
        malformed["error"]["message"],
        "Invalid JSON payload in upstream SSE frame"
    );
    cleanup(&account_env, &client_env);
}

#[tokio::test]
async fn invalid_utf8_sse_becomes_an_immediate_protocol_error() {
    let _env = ENV_LOCK.lock().await;
    let mut body = b"data: \xff\n\n".to_vec();
    body.extend_from_slice(
        b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"ignored\"}}\n\n",
    );
    let (upstream, _) = start_upstream(vec![Reply::StaticBytes {
        status: StatusCode::OK,
        content_type: "text/event-stream",
        body,
    }])
    .await;
    let (gateway, account_env, client_env, _env_cleanup) =
        start_gateway(&upstream, "INVALID_UTF8").await;
    let mut socket = connect(&gateway, "/v1/responses").await;

    send_create(&mut socket, "invalid-utf8").await;
    let error = next_json(&mut socket).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["status"], 502);
    assert_eq!(error["error"]["code"], "websocket_protocol_error");
    assert_eq!(
        error["error"]["message"],
        "Invalid UTF-8 in upstream SSE frame"
    );
    cleanup(&account_env, &client_env);
}

#[tokio::test]
async fn upstream_error_projects_only_safe_headers() {
    let _env = ENV_LOCK.lock().await;
    let (upstream, _) = start_upstream(vec![Reply::Static {
        status: StatusCode::BAD_REQUEST,
        content_type: "application/json",
        body: r#"{"error":{"type":"invalid_request_error","code":"bad_input","message":"bad request"}}"#.to_string(),
        headers: vec![
            ("retry-after", "9"),
            ("x-request-id", "req-1"),
            ("x-codex-primary-used-percent", "42"),
            ("set-cookie", "secret=cookie"),
        ],
    }])
    .await;
    let (gateway, account_env, client_env, _env_cleanup) =
        start_gateway(&upstream, "HEADERS").await;
    let mut socket = connect(&gateway, "/v1/responses").await;
    send_create(&mut socket, "headers").await;
    let error = next_json(&mut socket).await;
    assert_eq!(error["status"], 400);
    assert_eq!(error["error"]["code"], "bad_input");
    assert_eq!(error["headers"]["retry-after"], "9");
    assert_eq!(error["headers"]["x-request-id"], "req-1");
    assert_eq!(error["headers"]["x-codex-primary-used-percent"], "42");
    assert!(error["headers"].get("set-cookie").is_none());
    cleanup(&account_env, &client_env);
}

#[tokio::test]
async fn replacement_and_disconnect_drop_active_upstream_bodies() {
    let _env = ENV_LOCK.lock().await;
    let first_started = Arc::new(Notify::new());
    let first_dropped = Arc::new(Notify::new());
    let second_started = Arc::new(Notify::new());
    let second_dropped = Arc::new(Notify::new());
    let replies = vec![
        Reply::Held {
            initial: "data: {\"type\":\"response.created\",\"marker\":\"first\"}\n\n".to_string(),
            started: first_started.clone(),
            dropped: first_dropped.clone(),
        },
        Reply::Static {
            status: StatusCode::OK,
            content_type: "text/event-stream",
            body: "data: {\"type\":\"response.completed\",\"marker\":\"second\"}\n\n".to_string(),
            headers: Vec::new(),
        },
        Reply::Held {
            initial: "data: {\"type\":\"response.created\",\"marker\":\"disconnect\"}\n\n"
                .to_string(),
            started: second_started.clone(),
            dropped: second_dropped.clone(),
        },
    ];
    let (upstream, _) = start_upstream(replies).await;
    let (gateway, account_env, client_env, _env_cleanup) = start_gateway(&upstream, "CANCEL").await;

    let mut socket = connect(&gateway, "/v1/responses").await;
    send_create(&mut socket, "first").await;
    tokio::time::timeout(Duration::from_secs(5), first_started.notified())
        .await
        .unwrap();
    assert_eq!(next_json(&mut socket).await["marker"], "first");
    send_create(&mut socket, "second").await;
    tokio::time::timeout(Duration::from_secs(5), first_dropped.notified())
        .await
        .expect("replacement did not drop the first upstream body");
    let terminal = next_json(&mut socket).await;
    assert_eq!(terminal["type"], "response.completed");
    assert_eq!(terminal["marker"], "second");

    send_create(&mut socket, "disconnect").await;
    tokio::time::timeout(Duration::from_secs(5), second_started.notified())
        .await
        .unwrap();
    drop(socket);
    tokio::time::timeout(Duration::from_secs(5), second_dropped.notified())
        .await
        .expect("disconnect did not drop the active upstream body");
    cleanup(&account_env, &client_env);
}
