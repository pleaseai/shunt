use std::{sync::mpsc, time::Duration};

use axum::{body::Body, http::Request};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use shunt::config::{CodexEndpointConfig, ModelConfig, PoolConfig};
use tower::ServiceExt;

use super::support::*;

fn invalid_header_token() -> String {
    let payload = json!({
        "exp": 4_102_444_800_u64,
        "https://api.openai.com/auth": {"chatgpt_account_id": "invalid\nheader"}
    });
    format!("x.{}.y", URL_SAFE_NO_PAD.encode(payload.to_string()))
}

#[tokio::test]
async fn codex_local_failure_still_advances_a_generic_chain() {
    for invalid_header in [false, true] {
        for websocket in [false, true] {
            for absent in [false, true] {
                let mut fixture = Fixture::new(0, false, 1).await;
                fixture.success().await;
                let variable = fixture.account("codex", 0).token_env.clone().unwrap();
                if invalid_header {
                    fixture.put_env(&variable, invalid_header_token());
                } else {
                    fixture.clear_env(&variable);
                }
                fixture.config.providers.get_mut("codex").unwrap().websocket = websocket;
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
                fixture.config.upstreams = ["codex", "anthropic"]
                    .into_iter()
                    .map(|name| {
                        let provider = &fixture.config.providers[name];
                        serde_json::from_value(json!({
                            "name": name, "kind": provider.kind, "base_url": provider.base_url,
                            "auth": {"mode": provider.auth, "accounts": provider.accounts},
                            "request_compression": false, "retry": {"max_retries": 0},
                            "websocket": provider.websocket
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
                            ("codex".into(), PAIRS[0].1.into()),
                            ("anthropic".into(), PAIRS[0].0.into()),
                        ]
                        .into(),
                    ),
                    stage_router: None,
                });
                let gateway = fixture.gateway().await;

                let response = gateway.post().await;

                assert_eq!(response.status(), 200);
                assert_headers(&response, "anthropic", PAIRS[0].0);
                assert_eq!(
                    response.json::<Value>().await.unwrap()["content"][0]["text"],
                    "hello"
                );
                assert!(fixture.hits("codex").await.is_empty());
                let requests = fixture.hits("anthropic").await;
                assert_eq!(requests.len(), 1);
                assert_eq!(upstream_body(&requests[0])["model"], PAIRS[0].0);
            }
        }
    }
}

#[test]
fn quota_observed_during_credential_io_does_not_authorize_a_provider_switch() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        for invalid_header in [false, true] {
            for websocket in [false, true] {
                let mut fixture = Fixture::new(0, false, 1).await;
                fixture.success().await;
                fixture.config.providers.get_mut("codex").unwrap().websocket = websocket;
                let account = &mut fixture.config.providers.get_mut("codex").unwrap().accounts[0];
                let missing_path = std::env::temp_dir().join(format!(
                    "shunt-missing-credential-{}.json",
                    account.token_env.as_ref().unwrap()
                ));
                assert!(!missing_path.exists());
                if invalid_header {
                    std::fs::write(
                        &missing_path,
                        json!({"tokens": {
                            "access_token": invalid_header_token()
                        }})
                        .to_string(),
                    )
                    .unwrap();
                }
                account.token_env = None;
                account.credentials = Some(missing_path.to_string_lossy().into_owned());
                fixture.config.server.pool = Some(PoolConfig {
                    ramp_initial_concurrency: Some(1),
                    ..Default::default()
                });
                let gateway = fixture.gateway().await;
                let account = fixture.account("codex", 0);
                let pool = &gateway.state.accounts;
                assert!(!pool.strict_weekly_exhausted("codex", std::slice::from_ref(account)));

                // Occupy the sole I/O worker before the request starts credential resolution.
                let (started_tx, started_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = mpsc::channel();
                let barrier = tokio::task::spawn_blocking(move || {
                    let _ = started_tx.send(());
                    let _ = release_rx.recv();
                });
                started_rx.await.unwrap();
                let response = gateway.app.clone().oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/messages")
                        .header("content-type", "application/json")
                        .header("x-shunt-token", CLIENT_TOKEN)
                        .body(Body::from(request_body(false).to_string()))
                        .unwrap(),
                );
                tokio::pin!(response);
                assert!(futures_util::poll!(&mut response).is_pending());
                assert!(
                    pool.clone().try_admit("codex", account, 1, false).is_none(),
                    "the adapter must hold admission while credential I/O waits"
                );
                assert!(!pool.strict_weekly_exhausted("codex", std::slice::from_ref(account)));

                let mut alias = account.clone();
                alias.name = "quota-observer-alias".into();
                let mut headers = reqwest::header::HeaderMap::new();
                headers.insert("x-codex-secondary-window-minutes", "10080".parse().unwrap());
                headers.insert("x-codex-secondary-used-percent", "100".parse().unwrap());
                headers.insert(
                    "x-codex-secondary-reset-at",
                    reset_time().to_string().parse().unwrap(),
                );
                pool.note_codex_quota("codex-observer", &alias, &headers);
                assert!(
                    pool.strict_weekly_exhausted("codex", std::slice::from_ref(account)),
                    "the alias must update the same physical identity before resolution resumes"
                );

                release_tx.send(()).unwrap();
                barrier.await.unwrap();
                let response = tokio::time::timeout(Duration::from_secs(5), response)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(response.status(), 502);
                assert_eq!(response.headers()["x-gateway-upstream"], "codex");
                assert_eq!(response.headers()["x-gateway-model"], ALIAS);
                assert_eq!(response.headers()["x-gateway-upstream-model"], PAIRS[0].1);
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(
                    body["error"]["message"],
                    "all Codex OAuth accounts failed before receiving an upstream response"
                );
                assert!(pool.strict_weekly_exhausted("codex", std::slice::from_ref(account)));
                assert!(fixture.hits("codex").await.is_empty());
                assert!(fixture.hits("anthropic").await.is_empty());
                assert!(
                    pool.clone().try_admit("codex", account, 1, false).is_some(),
                    "the failed credential resolution must release admission"
                );
                if invalid_header {
                    std::fs::remove_file(missing_path).unwrap();
                }
            }
        }
    });
}

#[tokio::test]
async fn inbound_codex_keeps_its_local_header_error_without_weekly_fallback() {
    let mut fixture = Fixture::new(0, false, 1).await;
    fixture.config.server.codex_endpoint = Some(CodexEndpointConfig {
        provider: "codex".into(),
        routes: vec![],
    });
    fixture.success().await;
    let variable = fixture.account("codex", 0).token_env.clone().unwrap();
    fixture.put_env(&variable, invalid_header_token());
    let gateway = fixture.gateway().await;
    fixture.exhaust(&gateway, "codex");

    let response = reqwest::Client::new()
        .post(format!("{}/responses", gateway.base_url))
        .header("x-shunt-token", CLIENT_TOKEN)
        .json(&json!({"model": PAIRS[0].1, "input": "hello"}))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 502);
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body["error"]["message"],
        "all Codex OAuth accounts failed before receiving an upstream response"
    );
    assert_eq!(body["error"]["type"], "api_error");
    assert!(body.get("type").is_none());
    assert!(fixture.hits("codex").await.is_empty());
    assert!(fixture.hits("anthropic").await.is_empty());
}
