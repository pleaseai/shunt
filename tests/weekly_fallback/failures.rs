use serde_json::{json, Value};
use wiremock::{
    matchers::{body_partial_json, method},
    Mock, ResponseTemplate,
};

use super::support::*;

#[tokio::test]
async fn fable_upstream_failures_use_opus_once_without_a_provider_switch() {
    for status in [400, 401, 403, 404, 429, 500] {
        let fixture = Fixture::new(0, true, 1).await;
        let failure = ResponseTemplate::new(status)
            .insert_header("anthropic-ratelimit-unified-7d_oi-status", "rejected")
            .set_body_json(json!({"error": {"message": "synthetic Fable rejection"}}));
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"model": PAIRS[0].0})))
            .respond_with(failure)
            .expect(1)
            .mount(&fixture.claude)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"model": PAIRS[1].0})))
            .respond_with(claude_success())
            .expect(1)
            .mount(&fixture.claude)
            .await;
        let gateway = fixture.gateway().await;
        let response = gateway.post().await;
        assert_eq!(response.status(), 200, "Fable status: {status}");
        assert_headers(&response, "anthropic", PAIRS[1].0);
        response.bytes().await.unwrap();
        fixture.claude.verify().await;
        assert!(fixture.hits("codex").await.is_empty());
    }
}

#[tokio::test]
async fn missing_claude_token_keeps_fable_without_an_opus_attempt() {
    let mut fixture = Fixture::new(0, true, 1).await;
    let variable = fixture.account("anthropic", 0).token_env.clone().unwrap();
    fixture.clear_env(&variable);
    let gateway = fixture.gateway().await;

    let response = gateway.post().await;

    assert_eq!(response.status(), 502);
    assert_headers(&response, "anthropic", PAIRS[0].0);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("all Claude OAuth accounts failed"));
    assert!(fixture.hits("anthropic").await.is_empty());
    assert!(fixture.hits("codex").await.is_empty());
}

#[tokio::test]
async fn fable_transport_failure_still_attempts_opus() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut fixture = Fixture::new(0, true, 1).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    fixture
        .config
        .providers
        .get_mut("anthropic")
        .unwrap()
        .base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        tokio::time::timeout(std::time::Duration::from_secs(10), async move {
            let mut models = Vec::new();
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut received = Vec::new();
                let body = loop {
                    let mut chunk = [0; 4096];
                    let read = socket.read(&mut chunk).await.unwrap();
                    assert!(read > 0);
                    received.extend_from_slice(&chunk[..read]);
                    let Some(header_end) = received.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                        continue;
                    };
                    let headers = std::str::from_utf8(&received[..header_end]).unwrap();
                    let length: usize = headers.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
                    }).unwrap();
                    let body_start = header_end + 4;
                    if received.len() >= body_start + length {
                        break serde_json::from_slice::<Value>(&received[body_start..body_start + length]).unwrap();
                    }
                };
                models.push(body["model"].clone());
                if attempt == 1 {
                    let body = br#"{"type":"message","content":[{"type":"text","text":"hello"}]}"#;
                    let headers = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                    socket.write_all(headers.as_bytes()).await.unwrap();
                    socket.write_all(body).await.unwrap();
                }
            }
            models
        }).await.expect("both adapter attempts must finish")
    });
    let gateway = fixture.gateway().await;

    let response = gateway.post().await;

    assert_eq!(response.status(), 200);
    assert_headers(&response, "anthropic", PAIRS[1].0);
    assert_eq!(
        response.json::<Value>().await.unwrap()["content"][0]["text"],
        "hello"
    );
    assert_eq!(
        server.await.unwrap(),
        vec![json!(PAIRS[0].0), json!(PAIRS[1].0)]
    );
    assert!(fixture.hits("codex").await.is_empty());
}

#[tokio::test]
async fn generic_chains_preserve_pooled_local_error_advance_without_weekly_policy() {
    use shunt::config::{AuthMode, ModelConfig};

    for auth in [AuthMode::ClaudeOauth, AuthMode::KimiOauth] {
        for absent in [false, true] {
            let mut fixture = Fixture::new(0, true, 1).await;
            fixture.success().await;
            let variable = fixture.account("anthropic", 0).token_env.clone().unwrap();
            fixture.clear_env(&variable);
            fixture.config.providers.get_mut("anthropic").unwrap().auth = auth;
            if absent {
                fixture.config.server.weekly_fallback = None;
            } else {
                fixture
                    .config
                    .server
                    .weekly_fallback
                    .as_mut()
                    .unwrap()
                    .enabled = false;
            }
            fixture.config.upstreams = ["anthropic", "codex"]
                .into_iter()
                .map(|name| {
                    let provider = &fixture.config.providers[name];
                    serde_json::from_value(json!({
                        "name": name, "kind": provider.kind, "base_url": provider.base_url,
                        "auth": {"mode": provider.auth, "accounts": provider.accounts},
                        "request_compression": false, "retry": {"max_retries": 0}
                    }))
                    .unwrap()
                })
                .collect();
            fixture.config.providers.clear();
            fixture.config.routes.clear();
            fixture.config.models.push(ModelConfig {
                id: "weekly-alias".into(),
                display_name: None,
                upstream_model: Some(
                    [
                        ("anthropic".into(), PAIRS[0].0.into()),
                        ("codex".into(), PAIRS[0].1.into()),
                    ]
                    .into(),
                ),
                stage_router: None,
            });
            let gateway = fixture.gateway().await;

            let response = gateway.post().await;

            assert_eq!(response.status(), 200);
            assert_headers(&response, "codex", PAIRS[0].1);
            assert_eq!(
                response.json::<Value>().await.unwrap()["content"][0]["text"],
                "hello"
            );
            assert!(fixture.hits("anthropic").await.is_empty());
            let requests = fixture.hits("codex").await;
            assert_eq!(requests.len(), 1);
            assert_eq!(upstream_body(&requests[0])["model"], PAIRS[0].1);
        }
    }
}

#[tokio::test]
async fn opus_failure_retains_the_original_fable_astra_pair() {
    let fixture = Fixture::new(0, true, 1).await;
    Mock::given(body_partial_json(json!({"model": PAIRS[0].0})))
        .respond_with(ResponseTemplate::new(400))
        .expect(1)
        .mount(&fixture.claude)
        .await;
    Mock::given(body_partial_json(json!({"model": PAIRS[1].0})))
        .respond_with(quota_failure("anthropic", 403))
        .expect(1)
        .mount(&fixture.claude)
        .await;
    Mock::given(method("POST"))
        .respond_with(codex_success())
        .mount(&fixture.codex)
        .await;
    let gateway = fixture.gateway().await;
    let response = gateway.post().await;
    assert_eq!(response.status(), 200);
    assert_headers(&response, "codex", PAIRS[0].1);
    response.bytes().await.unwrap();
    assert_eq!(
        upstream_body(&fixture.hits("codex").await[0])["model"],
        PAIRS[0].1
    );
    fixture.claude.verify().await;
}

#[tokio::test]
async fn astra_switch_can_use_opus_but_never_returns_to_codex() {
    for opus_status in [200, 400] {
        let fixture = Fixture::new(0, false, 1).await;
        Mock::given(body_partial_json(json!({"model": PAIRS[0].0})))
            .respond_with(ResponseTemplate::new(400))
            .expect(1)
            .mount(&fixture.claude)
            .await;
        Mock::given(body_partial_json(json!({"model": PAIRS[1].0})))
            .respond_with(if opus_status == 200 {
                claude_success()
            } else {
                ResponseTemplate::new(opus_status)
                    .set_body_json(json!({"error":{"message":"opus failure"}}))
            })
            .expect(1)
            .mount(&fixture.claude)
            .await;
        let gateway = fixture.gateway().await;
        fixture.exhaust(&gateway, "codex");
        let response = gateway.post().await;
        assert_eq!(response.status(), opus_status);
        assert_headers(&response, "anthropic", PAIRS[1].0);
        response.bytes().await.unwrap();
        fixture.claude.verify().await;
        assert!(fixture.hits("codex").await.is_empty());
    }
}

#[tokio::test]
async fn non_fable_failure_without_strict_evidence_remains_terminal() {
    for from_claude in [true, false] {
        let fixture = Fixture::new(1, from_claude, 1).await;
        let (server, primary, destination) = if from_claude {
            (&fixture.claude, "anthropic", "codex")
        } else {
            (&fixture.codex, "codex", "anthropic")
        };
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(503)
                    .set_body_json(json!({"error":{"message":"temporary failure"}})),
            )
            .mount(server)
            .await;
        let gateway = fixture.gateway().await;
        let response = gateway.post().await;
        assert_eq!(response.status(), 503);
        response.bytes().await.unwrap();
        assert_eq!(fixture.hits(primary).await.len(), 1);
        assert!(fixture.hits(destination).await.is_empty());
    }
}

#[tokio::test]
async fn primary_resolver_error_does_not_treat_old_identity_evidence_as_exhaustion() {
    for from_claude in [true, false] {
        let mut fixture = Fixture::new(2, from_claude, 1).await;
        let provider = if from_claude { "anthropic" } else { "codex" };
        fixture
            .config
            .providers
            .get_mut(provider)
            .unwrap()
            .account_scope = vec!["missing-weekly-account".into()];
        let gateway = fixture.gateway().await;
        fixture.exhaust(&gateway, provider);
        let response = gateway.post().await;
        assert_eq!(response.status(), if from_claude { 401 } else { 502 });
        assert_eq!(response.headers()["x-gateway-upstream"], provider);
        let body: Value = response.json().await.unwrap();
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing store account"));
        assert!(fixture.hits("anthropic").await.is_empty());
        assert!(fixture.hits("codex").await.is_empty());
    }
}

#[tokio::test]
async fn destination_resolver_error_is_terminal_after_the_primary_is_skipped() {
    for from_claude in [true, false] {
        let mut fixture = Fixture::new(2, from_claude, 1).await;
        let (primary, destination) = if from_claude {
            ("anthropic", "codex")
        } else {
            ("codex", "anthropic")
        };
        fixture
            .config
            .providers
            .get_mut(destination)
            .unwrap()
            .account_scope = vec!["missing-weekly-account".into()];
        let gateway = fixture.gateway().await;
        fixture.exhaust(&gateway, primary);
        fixture.exhaust(&gateway, destination);
        let response = gateway.post().await;
        assert_eq!(response.status(), if from_claude { 502 } else { 401 });
        assert_eq!(response.headers()["x-gateway-upstream"], destination);
        let body: Value = response.json().await.unwrap();
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("missing store account"));
        assert!(fixture.hits("anthropic").await.is_empty());
        assert!(fixture.hits("codex").await.is_empty());
    }
}

#[tokio::test]
async fn empty_or_disabled_claude_pool_cannot_authorize_a_switch() {
    for disabled in [false, true] {
        let mut fixture = Fixture::new(0, true, 1).await;
        if disabled {
            fixture
                .config
                .providers
                .get_mut("anthropic")
                .unwrap()
                .accounts[0]
                .disabled = true;
        } else {
            fixture
                .config
                .providers
                .get_mut("anthropic")
                .unwrap()
                .accounts
                .clear();
        }
        let gateway = fixture.gateway().await;
        fixture.exhaust(&gateway, "anthropic");
        let response = gateway.post().await;
        assert_eq!(response.status(), 401);
        assert_eq!(response.headers()["x-gateway-upstream"], "anthropic");
        let body: Value = response.json().await.unwrap();
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains(if disabled { "disabled" } else { "no accounts" }));
        assert!(fixture.hits("anthropic").await.is_empty());
        assert!(fixture.hits("codex").await.is_empty());
    }
}

#[tokio::test]
async fn local_destination_error_is_terminal_without_replaying_the_primary() {
    let mut fixture = Fixture::new(0, true, 1).await;
    fixture.config.providers.get_mut("codex").unwrap().accounts[0].disabled = true;
    let gateway = fixture.gateway().await;
    fixture.exhaust(&gateway, "anthropic");
    let response = gateway.post().await;
    assert_eq!(response.status(), 502);
    assert_headers(&response, "codex", PAIRS[0].1);
    response.bytes().await.unwrap();
    assert!(fixture.hits("anthropic").await.is_empty());
    assert!(fixture.hits("codex").await.is_empty());
}

#[tokio::test]
async fn an_error_after_successful_codex_headers_is_terminal_even_with_weekly_exhaustion() {
    let fixture = Fixture::new(0, false, 1).await;
    let response = quota_failure("codex", 200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(concat!(
            "event: response.created\ndata: {\"response\":{\"id\":\"resp_weekly\"}}\n\n",
            "event: response.output_text.delta\ndata: {\"delta\":\"partial\"}\n\n",
            "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"terminal stream failure\"}}}\n\n"
        ));
    Mock::given(method("POST"))
        .respond_with(response)
        .mount(&fixture.codex)
        .await;
    let gateway = fixture.gateway().await;
    let response = gateway.post().await;
    // An in-stream `rate_limit_exceeded` surfaces as `429 rate_limit_error`
    // (issue #463), and it stays terminal: the upstream already accepted the
    // turn, so the weekly policy must not replay it on the paired provider.
    assert_eq!(response.status(), 429);
    assert_headers(&response, "codex", PAIRS[0].1);
    let body = response.json::<Value>().await.unwrap();
    assert_eq!(body["error"]["type"], "rate_limit_error");
    assert_eq!(body["error"]["message"], "terminal stream failure");
    assert!(gateway
        .state
        .accounts
        .strict_weekly_exhausted("codex", &fixture.config.providers["codex"].accounts));
    assert_eq!(fixture.hits("codex").await.len(), 1);
    assert!(fixture.hits("anthropic").await.is_empty());
}
