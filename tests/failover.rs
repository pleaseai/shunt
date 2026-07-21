use std::{
    collections::BTreeMap,
    io::ErrorKind,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use reqwest::StatusCode;
use serde_json::{json, Value};
use shunt::{
    config::{
        ApiKeyHeader, AuthMap, AuthMode, Config, CountTokens, InboundAuthConfig, ModelConfig,
        ProviderKind, RetryConfig, UpstreamAuth, UpstreamConfig,
    },
    server,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use wiremock::{
    matchers::{header, method, path},
    Match, Mock, MockServer, Request, ResponseTemplate,
};

const CLIENT_MODEL: &str = "failover-model";

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

fn disabled_retry() -> RetryConfig {
    RetryConfig {
        max_retries: 0,
        ..RetryConfig::default()
    }
}

fn upstream(
    name: &str,
    base_url: String,
    kind: ProviderKind,
    auth: UpstreamAuth,
) -> UpstreamConfig {
    UpstreamConfig {
        name: name.to_string(),
        provider: None,
        kind: Some(kind),
        base_url: Some(base_url),
        auth: Some(auth),
        effort: None,
        count_tokens: CountTokens::Tiktoken,
        websocket: false,
        tool_search: false,
        retry: disabled_retry(),
    }
}

fn passthrough(name: &str, base_url: String) -> UpstreamConfig {
    upstream(
        name,
        base_url,
        ProviderKind::Anthropic,
        UpstreamAuth::Shorthand(AuthMode::Passthrough),
    )
}

fn chain_config(upstreams: Vec<UpstreamConfig>, mappings: &[(&str, &str)]) -> Config {
    let mut config = Config::default();
    config.server.default_provider = upstreams
        .first()
        .expect("test chains are non-empty")
        .name
        .clone();
    config.providers.clear();
    config.upstreams = upstreams;
    config.models = vec![ModelConfig {
        id: CLIENT_MODEL.to_string(),
        display_name: None,
        upstream_model: Some(
            mappings
                .iter()
                .map(|(name, model)| ((*name).to_string(), (*model).to_string()))
                .collect::<BTreeMap<_, _>>(),
        ),
    }];
    config
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

async fn post(gateway: &TestGateway) -> reqwest::Response {
    post_path(gateway, "/v1/messages", &[]).await
}

async fn spawn_truncated_http_upstream(
    content_type: &'static str,
    partial_body: &'static [u8],
) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let server_hits = hits.clone();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        server_hits.fetch_add(1, Ordering::SeqCst);
        let mut request = [0_u8; 8192];
        let _ = socket.read(&mut request).await;
        let declared_length = partial_body.len() + 64;
        let headers = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {declared_length}\r\nconnection: close\r\n\r\n"
        );
        socket.write_all(headers.as_bytes()).await.unwrap();
        socket.write_all(partial_body).await.unwrap();
        socket.flush().await.unwrap();
    });
    (format!("http://{addr}"), hits)
}

async fn post_path(
    gateway: &TestGateway,
    path: &str,
    headers: &[(&str, &str)],
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{}{path}", gateway.base_url))
        .header("content-type", "application/json")
        .body(
            json!({
                "model": CLIENT_MODEL,
                "max_tokens": 16,
                "messages": [{"role": "user", "content": "hi"}]
            })
            .to_string(),
        );
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    request.send().await.unwrap()
}

fn assert_gateway_headers(response: &reqwest::Response, upstream: &str, upstream_model: &str) {
    assert_eq!(response.headers()["x-gateway-upstream"], upstream);
    assert_eq!(response.headers()["x-gateway-model"], CLIENT_MODEL);
    assert_eq!(
        response.headers()["x-gateway-upstream-model"],
        upstream_model
    );
}

#[tokio::test]
async fn chain_order_stops_at_first_healthy_upstream() {
    if !can_bind_loopback() {
        return;
    }
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"winner":"first"}"#))
        .expect(1)
        .mount(&first)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&second)
        .await;
    let config = chain_config(
        vec![
            passthrough("first", first.uri()),
            passthrough("second", second.uri()),
        ],
        &[("first", "model-a"), ("second", "model-b")],
    );
    let gateway = start_gateway(config).await;

    let response = post(&gateway).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_gateway_headers(&response, "first", "model-a");
    assert_eq!(response.text().await.unwrap(), r#"{"winner":"first"}"#);
    first.verify().await;
    second.verify().await;
}

#[tokio::test]
async fn every_advance_status_reaches_the_next_upstream() {
    if !can_bind_loopback() {
        return;
    }
    for status in [401, 403, 404, 429, 500, 503] {
        let first = MockServer::start().await;
        let second = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(status).set_body_string(format!("failure-{status}")),
            )
            .expect(1)
            .mount(&first)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("healthy"))
            .expect(1)
            .mount(&second)
            .await;
        let config = chain_config(
            vec![
                passthrough("first", first.uri()),
                passthrough("second", second.uri()),
            ],
            &[("first", "model-a"), ("second", "model-b")],
        );
        let gateway = start_gateway(config).await;

        let response = post(&gateway).await;

        assert_eq!(response.status(), StatusCode::OK, "status {status}");
        assert_gateway_headers(&response, "second", "model-b");
        assert_eq!(response.text().await.unwrap(), "healthy");
        first.verify().await;
        second.verify().await;
    }
}

#[tokio::test]
async fn connect_failure_advances_but_400_returns_immediately() {
    if !can_bind_loopback() {
        return;
    }
    let unavailable = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let unavailable_url = format!("http://{}", unavailable.local_addr().unwrap());
    drop(unavailable);
    let healthy = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("after-connect"))
        .expect(1)
        .mount(&healthy)
        .await;
    let config = chain_config(
        vec![
            passthrough("offline", unavailable_url),
            passthrough("healthy", healthy.uri()),
        ],
        &[("offline", "model-a"), ("healthy", "model-b")],
    );
    let gateway = start_gateway(config).await;
    let response = post(&gateway).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_gateway_headers(&response, "healthy", "model-b");
    healthy.verify().await;

    let bad = MockServer::start().await;
    let skipped = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string("client-error"))
        .expect(1)
        .mount(&bad)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&skipped)
        .await;
    let config = chain_config(
        vec![
            passthrough("bad", bad.uri()),
            passthrough("skipped", skipped.uri()),
        ],
        &[("bad", "model-a"), ("skipped", "model-b")],
    );
    let gateway = start_gateway(config).await;
    let response = post(&gateway).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_gateway_headers(&response, "bad", "model-a");
    assert_eq!(response.text().await.unwrap(), "client-error");
    bad.verify().await;
    skipped.verify().await;
}

#[tokio::test]
async fn exhausted_chain_returns_highest_priority_failure() {
    if !can_bind_loopback() {
        return;
    }
    for (statuses, expected_status, expected_body, expected_upstream) in [
        ((500, 429), 429, "second-429", "second"),
        ((404, 500), 404, "first-404", "first"),
    ] {
        let first = MockServer::start().await;
        let second = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(statuses.0).set_body_string(format!("first-{}", statuses.0)),
            )
            .mount(&first)
            .await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(statuses.1).set_body_string(format!("second-{}", statuses.1)),
            )
            .mount(&second)
            .await;
        let config = chain_config(
            vec![
                passthrough("first", first.uri()),
                passthrough("second", second.uri()),
            ],
            &[("first", "model-a"), ("second", "model-b")],
        );
        let gateway = start_gateway(config).await;

        let response = post(&gateway).await;

        assert_eq!(response.status().as_u16(), expected_status);
        let expected_model = if expected_upstream == "first" {
            "model-a"
        } else {
            "model-b"
        };
        assert_gateway_headers(&response, expected_upstream, expected_model);
        assert_eq!(response.text().await.unwrap(), expected_body);
    }
}

#[tokio::test]
async fn all_transport_failures_synthesize_anthropic_502_with_last_metadata() {
    if !can_bind_loopback() {
        return;
    }
    let first = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let first_url = format!("http://{}", first.local_addr().unwrap());
    drop(first);
    let second = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let second_url = format!("http://{}", second.local_addr().unwrap());
    drop(second);
    let config = chain_config(
        vec![
            passthrough("first", first_url),
            passthrough("second", second_url),
        ],
        &[("first", "model-a"), ("second", "model-b")],
    );
    let gateway = start_gateway(config).await;

    let response = post(&gateway).await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_gateway_headers(&response, "second", "model-b");
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body,
        json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "message": "all upstreams failed (2 attempted)"
            }
        })
    );
}

#[tokio::test]
async fn responses_raw_404_advances_despite_client_facing_502_mapping() {
    if !can_bind_loopback() {
        return;
    }
    let responses = MockServer::start().await;
    let anthropic = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(404).set_body_string(r#"{"message":"missing"}"#))
        .expect(1)
        .mount(&responses)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_string("fallback"))
        .expect(1)
        .mount(&anthropic)
        .await;
    let config = chain_config(
        vec![
            upstream(
                "responses",
                responses.uri(),
                ProviderKind::Responses,
                UpstreamAuth::Shorthand(AuthMode::Passthrough),
            ),
            passthrough("anthropic", anthropic.uri()),
        ],
        &[("responses", "gpt-test"), ("anthropic", "claude-test")],
    );
    let gateway = start_gateway(config).await;

    let response = post(&gateway).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_gateway_headers(&response, "anthropic", "claude-test");
    responses.verify().await;
    anthropic.verify().await;
}

#[tokio::test]
async fn responses_post_2xx_backend_error_stops_without_replaying_turn() {
    if !can_bind_loopback() {
        return;
    }
    let responses = MockServer::start().await;
    let skipped = MockServer::start().await;
    let failed_turn = concat!(
        "event: response.created\n",
        "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
        "event: response.failed\n",
        "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"first upstream accepted then failed\"}}}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(failed_turn))
        .expect(1)
        .mount(&responses)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("must not replay"))
        .expect(0)
        .mount(&skipped)
        .await;
    let config = chain_config(
        vec![
            upstream(
                "responses",
                responses.uri(),
                ProviderKind::Responses,
                UpstreamAuth::Shorthand(AuthMode::Passthrough),
            ),
            passthrough("skipped", skipped.uri()),
        ],
        &[("responses", "gpt-test"), ("skipped", "claude-test")],
    );
    let gateway = start_gateway(config).await;

    let response = post(&gateway).await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_gateway_headers(&response, "responses", "gpt-test");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(
        body["error"]["message"],
        "first upstream accepted then failed"
    );
    responses.verify().await;
    skipped.verify().await;
}

#[tokio::test]
async fn anthropic_truncated_200_body_stops_without_replaying_turn() {
    if !can_bind_loopback() {
        return;
    }
    let (first_url, first_hits) =
        spawn_truncated_http_upstream("application/json", br#"{"id":"partial""#).await;
    let skipped = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("must not replay"))
        .expect(0)
        .mount(&skipped)
        .await;
    let config = chain_config(
        vec![
            passthrough("anthropic", first_url),
            passthrough("skipped", skipped.uri()),
        ],
        &[("anthropic", "claude-test"), ("skipped", "claude-next")],
    );
    let gateway = start_gateway(config).await;

    let response = post(&gateway).await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_gateway_headers(&response, "anthropic", "claude-test");
    assert_eq!(first_hits.load(Ordering::SeqCst), 1);
    skipped.verify().await;
}

#[tokio::test]
async fn responses_truncated_200_body_stops_without_replaying_turn() {
    if !can_bind_loopback() {
        return;
    }
    let partial = b"event: response.created\ndata: {\"response\":{\"id\":\"partial\"}}\n\n";
    let (first_url, first_hits) = spawn_truncated_http_upstream("text/event-stream", partial).await;
    let skipped = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("must not replay"))
        .expect(0)
        .mount(&skipped)
        .await;
    let config = chain_config(
        vec![
            upstream(
                "responses",
                first_url,
                ProviderKind::Responses,
                UpstreamAuth::Shorthand(AuthMode::Passthrough),
            ),
            passthrough("skipped", skipped.uri()),
        ],
        &[("responses", "gpt-test"), ("skipped", "claude-test")],
    );
    let gateway = start_gateway(config).await;

    let response = post(&gateway).await;

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert_gateway_headers(&response, "responses", "gpt-test");
    assert_eq!(first_hits.load(Ordering::SeqCst), 1);
    skipped.verify().await;
}

#[tokio::test]
async fn mixed_chain_is_gated_and_strips_credentials_per_attempt() {
    if !can_bind_loopback() {
        return;
    }
    let key_env = format!("SHUNT_FAILOVER_KEY_{}", std::process::id());
    let tokens_env = format!("SHUNT_FAILOVER_CLIENT_{}", std::process::id());
    std::env::set_var(&key_env, "upstream-key");
    std::env::set_var(&tokens_env, "alice:client-token");
    let passthrough_server = MockServer::start().await;
    let injected_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer client-upstream"))
        .and(header("x-api-key", "client-api-key"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&passthrough_server)
        .await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer upstream-key"))
        .and(HeaderAbsent("x-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string("credentialed"))
        .expect(1)
        .mount(&injected_server)
        .await;
    let mut config = chain_config(
        vec![
            passthrough("passthrough", passthrough_server.uri()),
            upstream(
                "credentialed",
                injected_server.uri(),
                ProviderKind::Anthropic,
                UpstreamAuth::Map(AuthMap::ApiKey {
                    env: Some(key_env.clone()),
                    header: ApiKeyHeader::Bearer,
                }),
            ),
        ],
        &[("passthrough", "model-a"), ("credentialed", "model-b")],
    );
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: tokens_env.clone(),
    });
    let gateway = start_gateway(config).await;

    let denied = post(&gateway).await;
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let response = post_path(
        &gateway,
        "/v1/messages",
        &[
            ("x-shunt-token", "client-token"),
            ("authorization", "Bearer client-upstream"),
            ("x-api-key", "client-api-key"),
        ],
    )
    .await;

    std::env::remove_var(key_env);
    std::env::remove_var(tokens_env);
    assert_eq!(response.status(), StatusCode::OK);
    assert_gateway_headers(&response, "credentialed", "model-b");
    passthrough_server.verify().await;
    injected_server.verify().await;
}

#[tokio::test]
async fn passthrough_failover_does_not_replay_client_credential_to_next_host() {
    if !can_bind_loopback() {
        return;
    }
    // Two passthrough upstreams on different hosts (origins). The caller's
    // credential is origin-specific to the first upstream, so on failover to a
    // different origin it must not be replayed: the second attempt fails closed
    // with the credential stripped.
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer client-upstream"))
        .and(header("x-api-key", "client-api-key"))
        .respond_with(ResponseTemplate::new(500).set_body_string("first-500"))
        .expect(1)
        .mount(&first)
        .await;
    Mock::given(method("POST"))
        .and(HeaderAbsent("authorization"))
        .and(HeaderAbsent("x-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string("no-replay"))
        .expect(1)
        .mount(&second)
        .await;
    let config = chain_config(
        vec![
            passthrough("first", first.uri()),
            passthrough("second", second.uri()),
        ],
        &[("first", "model-a"), ("second", "model-b")],
    );
    let gateway = start_gateway(config).await;

    let response = post_path(
        &gateway,
        "/v1/messages",
        &[
            ("authorization", "Bearer client-upstream"),
            ("x-api-key", "client-api-key"),
        ],
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_gateway_headers(&response, "second", "model-b");
    assert_eq!(response.text().await.unwrap(), "no-replay");
    first.verify().await;
    second.verify().await;
}

#[tokio::test]
async fn passthrough_failover_keeps_client_credential_on_the_same_origin() {
    if !can_bind_loopback() {
        return;
    }
    // Two passthrough entries on the *same* origin (e.g. a model fallback on one
    // host). The caller's credential is valid for both, so failover must keep it:
    // the origin never changes, so no stripping happens. A mock that requires the
    // credential and expects two hits proves both same-origin attempts carried it.
    let origin = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer client-upstream"))
        .and(header("x-api-key", "client-api-key"))
        .respond_with(ResponseTemplate::new(500).set_body_string("same-origin-500"))
        .expect(2)
        .mount(&origin)
        .await;
    let config = chain_config(
        vec![
            passthrough("primary", origin.uri()),
            passthrough("fallback", origin.uri()),
        ],
        &[("primary", "model-a"), ("fallback", "model-b")],
    );
    let gateway = start_gateway(config).await;

    let response = post_path(
        &gateway,
        "/v1/messages",
        &[
            ("authorization", "Bearer client-upstream"),
            ("x-api-key", "client-api-key"),
        ],
    )
    .await;

    // Both same-origin attempts advanced on 500 with the credential intact (the
    // credential-requiring mock was hit twice), so the chain exhausts and returns
    // the best relayed failure — the first 500. If the fallback had been stripped
    // into a 401 the mock would have been hit only once and `verify` would fail.
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_gateway_headers(&response, "primary", "model-a");
    origin.verify().await;
}

#[tokio::test]
async fn count_tokens_uses_only_first_chain_element() {
    if !can_bind_loopback() {
        return;
    }
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(500).set_body_string("first-only"))
        .expect(1)
        .mount(&first)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&second)
        .await;
    let config = chain_config(
        vec![
            passthrough("first", first.uri()),
            passthrough("second", second.uri()),
        ],
        &[("first", "model-a"), ("second", "model-b")],
    );
    let gateway = start_gateway(config).await;

    let response = post_path(&gateway, "/v1/messages/count_tokens", &[]).await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_gateway_headers(&response, "first", "model-a");
    assert_eq!(response.text().await.unwrap(), "first-only");
    first.verify().await;
    second.verify().await;
}

#[tokio::test]
async fn legacy_single_element_chain_adds_gateway_headers() {
    if !can_bind_loopback() {
        return;
    }
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("legacy"))
        .expect(1)
        .mount(&upstream)
        .await;
    let mut config = Config::default();
    config.providers.get_mut("anthropic").unwrap().base_url = upstream.uri();
    config.models = vec![ModelConfig {
        id: CLIENT_MODEL.to_string(),
        display_name: None,
        upstream_model: Some(BTreeMap::from([(
            "anthropic".to_string(),
            "legacy-model".to_string(),
        )])),
    }];
    let gateway = start_gateway(config).await;

    let response = post(&gateway).await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_gateway_headers(&response, "anthropic", "legacy-model");
    assert_eq!(response.text().await.unwrap(), "legacy");
    upstream.verify().await;
}
