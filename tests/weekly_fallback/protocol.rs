use axum::{body::Body, http::Request};
use serde_json::{json, Value};
use shunt::{
    config::{CodexEndpointConfig, GatewayConfig, GatewayPolicyConfig, PoolConfig},
    gateway::{approval::Identity, jwt},
};
use tower::ServiceExt;
use wiremock::{
    matchers::{body_partial_json, method},
    Mock, ResponseTemplate,
};

use super::support::*;

#[tokio::test]
async fn authentication_precedes_quota_and_resolver_decisions() {
    let mut fixture = Fixture::new(0, true, 1).await;
    fixture
        .config
        .providers
        .get_mut("codex")
        .unwrap()
        .account_scope = vec!["missing".into()];
    let gateway = fixture.gateway().await;
    fixture.exhaust(&gateway, "anthropic");
    let response = reqwest::Client::new()
        .post(format!("{}/v1/messages", gateway.base_url))
        .json(&request_body(false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["type"],
        "authentication_error"
    );
    assert!(fixture.hits("anthropic").await.is_empty());
    assert!(fixture.hits("codex").await.is_empty());
}

#[tokio::test]
async fn managed_policy_authorizes_the_original_alias_for_every_possible_backend() {
    for from_claude in [true, false] {
        let mut fixture = Fixture::new(0, from_claude, 1).await;
        fixture.success().await;
        let secret = "0123456789abcdef0123456789abcdef";
        let secret_env = fixture.environment("JWT", secret);
        let users_env = fixture.environment("USERS", "test@example.invalid:synthetic-approval");
        fixture.config.server.gateway = Some(GatewayConfig {
            public_url: "https://weekly.example.invalid".into(),
            jwt_secret_env: Some(secret_env),
            users_env,
            token_ttl_seconds: Some(3600),
            trust_forwarded_for: false,
            policies: Some(vec![GatewayPolicyConfig {
                matcher: None,
                cli: toml::from_str("availableModels = [\"weekly-alias\"]").unwrap(),
            }]),
            telemetry: None,
            state_path: None,
            oidc: None,
            session: None,
        });
        let gateway = fixture.gateway().await;
        let (primary, destination, backend) = if from_claude {
            ("anthropic", "codex", PAIRS[0].1)
        } else {
            ("codex", "anthropic", PAIRS[0].0)
        };
        fixture.exhaust(&gateway, primary);
        let token = jwt::mint(
            &Identity {
                sub: "test".into(),
                email: "test@example.invalid".into(),
                name: "Test".into(),
            },
            "https://weekly.example.invalid",
            secret.as_bytes(),
            3600,
        );
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{}/v1/messages", gateway.base_url))
            .bearer_auth(&token)
            .json(&request_body(false))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_headers(&response, destination, backend);
        response.bytes().await.unwrap();
        let mut denied = request_body(false);
        denied["model"] = json!(backend);
        let response = client
            .post(format!("{}/v1/messages", gateway.base_url))
            .bearer_auth(&token)
            .json(&denied)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        response.bytes().await.unwrap();
        assert_eq!(fixture.hits(destination).await.len(), 1);
        assert!(fixture.hits(primary).await.is_empty());
    }
}

#[tokio::test]
async fn provider_switch_preserves_tools_and_uses_destination_credentials_and_defaults() {
    for from_claude in [true, false] {
        let mut fixture = Fixture::new(2, from_claude, 1).await;
        fixture.success().await;
        fixture.config.routes[0].effort = Some("low".into());
        fixture.config.routes[0].service_tier = Some("priority".into());
        fixture.config.providers.get_mut("codex").unwrap().effort = Some("high".into());
        fixture
            .config
            .providers
            .get_mut("codex")
            .unwrap()
            .service_tier = Some("flex".into());
        let gateway = fixture.gateway().await;
        let (primary, destination, backend) = if from_claude {
            ("anthropic", "codex", PAIRS[2].1)
        } else {
            ("codex", "anthropic", PAIRS[2].0)
        };
        fixture.exhaust(&gateway, primary);
        let body = json!({
            "model": ALIAS, "max_tokens": 32,
            "metadata": {"user_id": "{\"account_uuid\":\"original\",\"session_id\":\"tool-session\"}"},
            "output_config": {"effort": "medium"},
            "tools": [{"name": "lookup", "description": "Look up a value.",
                "input_schema": {"type": "object", "properties": {"key": {"type": "string"}}, "required": ["key"]}}],
            "messages": [
                {"role": "user", "content": "Find the answer."},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "tool_weekly", "name": "lookup", "input": {"key": "answer"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tool_weekly", "content": "42"}]}
            ]
        });
        let response = reqwest::Client::new()
            .post(format!("{}/v1/messages", gateway.base_url))
            .header("x-shunt-token", CLIENT_TOKEN)
            .header("authorization", format!("Bearer {CLIENT_TOKEN}"))
            .header("x-api-key", CLIENT_TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_headers(&response, destination, backend);
        response.bytes().await.unwrap();
        let requests = fixture.hits(destination).await;
        assert_eq!(requests.len(), 1);
        let sent = &requests[0];
        assert!(!sent.headers.contains_key("x-shunt-token"));
        assert!(!sent.headers.contains_key("x-api-key"));
        let expected_token =
            std::env::var(fixture.account(destination, 0).token_env.as_ref().unwrap()).unwrap();
        assert_eq!(
            sent.headers["authorization"],
            format!("Bearer {expected_token}")
        );
        let sent = upstream_body(sent);
        assert_eq!(sent["model"], backend);
        if from_claude {
            assert_eq!(sent["reasoning"]["effort"], "high");
            assert_eq!(sent["service_tier"], "flex");
            assert_eq!(sent["tools"][0]["name"], "lookup");
            assert_eq!(
                sent["tools"][0]["parameters"]["properties"]["key"]["type"],
                "string"
            );
            let input = sent["input"].as_array().unwrap();
            let call = input
                .iter()
                .find(|item| item["type"] == "function_call")
                .unwrap();
            assert_eq!(call["call_id"], "tool_weekly");
            assert_eq!(
                serde_json::from_str::<Value>(call["arguments"].as_str().unwrap()).unwrap(),
                json!({"key": "answer"})
            );
            let result = input
                .iter()
                .find(|item| item["type"] == "function_call_output")
                .unwrap();
            assert_eq!(result["call_id"], "tool_weekly");
            assert_eq!(result["output"], "42");
        } else {
            assert_eq!(sent["messages"], body["messages"]);
            assert_eq!(sent["tools"], body["tools"]);
            assert_eq!(sent["output_config"], body["output_config"]);
            let metadata: Value =
                serde_json::from_str(sent["metadata"]["user_id"].as_str().unwrap()).unwrap();
            assert_eq!(
                metadata["account_uuid"],
                fixture.account("anthropic", 0).uuid.as_deref().unwrap()
            );
            assert_eq!(metadata["session_id"], "tool-session");
        }
        assert!(fixture.hits(primary).await.is_empty());
    }
}

#[tokio::test]
async fn successful_and_partial_streams_never_replay_after_successful_headers() {
    for from_claude in [true, false] {
        for partial in [false, true] {
            let fixture = Fixture::new(0, from_claude, 1).await;
            let (primary, destination, backend, server) = if from_claude {
                ("anthropic", "codex", PAIRS[0].0, &fixture.claude)
            } else {
                ("codex", "anthropic", PAIRS[0].1, &fixture.codex)
            };
            let mut body = if from_claude {
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_weekly\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n".to_string()
            } else {
                "event: response.created\ndata: {\"response\":{\"id\":\"resp_weekly\"}}\n\nevent: response.output_item.added\ndata: {\"item\":{\"type\":\"message\"}}\n\nevent: response.output_text.delta\ndata: {\"delta\":\"hello\"}\n\n".to_string()
            };
            if !partial {
                body.push_str(if from_claude {"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"}
                    else {"event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"});
            }
            Mock::given(method("POST"))
                .respond_with(
                    quota_failure(primary, 200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(body),
                )
                .mount(server)
                .await;
            let gateway = fixture.gateway().await;
            let response = gateway.post_body(request_body(true)).await;
            assert_eq!(response.status(), 200);
            assert_headers(&response, primary, backend);
            assert!(response.text().await.unwrap().contains("hello"));
            assert_eq!(fixture.hits(primary).await.len(), 1);
            assert!(fixture.hits(destination).await.is_empty());
            assert!(gateway
                .state
                .accounts
                .strict_weekly_exhausted(primary, &fixture.config.providers[primary].accounts));
        }
    }
}

#[tokio::test]
async fn switch_releases_primary_admission_and_holds_destination_until_body_consumption() {
    let mut fixture = Fixture::new(2, true, 1).await;
    fixture.config.server.pool = Some(PoolConfig {
        ramp_initial_concurrency: Some(1),
        ..Default::default()
    });
    Mock::given(method("POST"))
        .respond_with(quota_failure("anthropic", 429))
        .mount(&fixture.claude)
        .await;
    Mock::given(method("POST"))
        .respond_with(codex_success())
        .mount(&fixture.codex)
        .await;
    let gateway = fixture.gateway().await;
    let response = gateway
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .header("x-shunt-token", CLIENT_TOKEN)
                .body(Body::from(request_body(true).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-gateway-upstream"], "codex");
    let pool = &gateway.state.accounts;
    let primary = pool
        .clone()
        .try_admit("anthropic", fixture.account("anthropic", 0), 1, false);
    assert!(
        primary.is_some(),
        "the failed attempt must release its admission"
    );
    let spare = pool
        .clone()
        .try_admit("codex", fixture.account("codex", 0), 1, false);
    assert!(spare.is_some(), "a success doubles the initial allowance");
    assert!(
        pool.clone()
            .try_admit("codex", fixture.account("codex", 0), 1, false)
            .is_none(),
        "the unread response must retain its admission"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(std::str::from_utf8(&body).unwrap().contains("hello"));
    assert!(
        pool.clone()
            .try_admit("codex", fixture.account("codex", 0), 1, false)
            .is_some(),
        "body consumption must release the destination admission"
    );
    drop(spare);
    drop(primary);
}

#[tokio::test]
async fn inbound_codex_ignores_messages_weekly_fallback() {
    let mut fixture = Fixture::new(0, false, 1).await;
    fixture.config.server.codex_endpoint = Some(CodexEndpointConfig {
        provider: "codex".into(),
        routes: vec![],
    });
    fixture.success().await;
    let gateway = fixture.gateway().await;
    fixture.exhaust(&gateway, "codex");
    let body = json!({"model": PAIRS[0].1, "input": "hello", "stream": true});
    let response = reqwest::Client::new()
        .post(format!("{}/responses", gateway.base_url))
        .header("x-shunt-token", CLIENT_TOKEN)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response
        .text()
        .await
        .unwrap()
        .contains("response.completed"));
    assert_eq!(upstream_body(&fixture.hits("codex").await[0]), body);
    assert!(fixture.hits("anthropic").await.is_empty());
}

#[tokio::test]
async fn fable_fallback_preserves_the_original_tool_body() {
    let fixture = Fixture::new(0, true, 1).await;
    Mock::given(body_partial_json(json!({"model": PAIRS[0].0})))
        .respond_with(ResponseTemplate::new(400))
        .mount(&fixture.claude)
        .await;
    Mock::given(body_partial_json(json!({"model": PAIRS[1].0})))
        .respond_with(claude_success())
        .mount(&fixture.claude)
        .await;
    let gateway = fixture.gateway().await;
    let mut body = request_body(false);
    body["tools"] = json!([{"name": "lookup", "input_schema": {"type": "object"}}]);
    body["messages"] = json!([
        {"role": "assistant", "content": [{"type": "tool_use", "id": "tool_weekly", "name": "lookup", "input": {}}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tool_weekly", "content": "answer"}]}
    ]);
    let response = gateway.post_body(body.clone()).await;
    assert_eq!(response.status(), 200);
    response.bytes().await.unwrap();
    let requests = fixture.hits("anthropic").await;
    assert_eq!(requests.len(), 2);
    for request in requests {
        let request = upstream_body(&request);
        assert_eq!(request["tools"], body["tools"]);
        assert_eq!(request["messages"], body["messages"]);
    }
}
