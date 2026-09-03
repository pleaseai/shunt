//! End-to-end: the Antigravity model id sent upstream comes from the account's
//! live catalog, not from a compiled-in suffix rule.
//!
//! The unit tests cover the resolution table; this proves the wiring, which is
//! what actually broke in the field. shunt 0.40.0 hard-coded
//! `{model}-{tier}`, and an account whose catalog publishes only
//! `gemini-3.8-flash-tiered` answered that id with
//! `404 Requested entity was not found` on every request.

use std::{io::ErrorKind, net::SocketAddr, time::Duration};

use serde_json::{json, Value};
use shunt::{config::Config, server};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

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

#[tokio::test]
async fn a_tiered_only_catalog_decides_the_model_id_and_the_thinking_level() {
    if !can_bind_loopback() {
        return;
    }

    let backend = MockServer::start().await;
    // The account's catalog: only the tiered form of this model exists, which
    // is exactly the shape that 404s under 0.40.0's hard-coded suffix. The
    // entries' inner `model` values are internal placeholders, so the keys are
    // the only wire ids.
    Mock::given(method("POST"))
        .and(path("/v1internal:fetchAvailableModels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": {
                "gemini-3.8-flash-tiered": {"model": "MODEL_PLACEHOLDER_M322"},
                "claude-sonnet-4-6": {"model": "MODEL_PLACEHOLDER_M118"}
            }
        })))
        .mount(&backend)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1internal:generateContent"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "response": {
                "candidates": [{
                    "content": {"parts": [{"text": "OK"}]},
                    "finishReason": "STOP"
                }]
            }
        })))
        .mount(&backend)
        .await;

    let dir = std::env::temp_dir().join(format!(
        "shunt-antigravity-catalog-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    // A cached `project_id` keeps discovery off the request path, so the only
    // calls the backend sees are the catalog fetch and the inference request.
    let credential_path = dir.join("antigravity-auth.json");
    let expiry = (std::time::SystemTime::now() + Duration::from_secs(3600))
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    std::fs::write(
        &credential_path,
        serde_json::to_vec(&json!({
            "access_token": "catalog-e2e-token",
            "refresh_token": "refresh",
            "expiry_date": expiry,
            "project_id": "proj-catalog"
        }))
        .unwrap(),
    )
    .unwrap();
    std::env::set_var("SHUNT_ANTIGRAVITY_AUTH_FILE", &credential_path);

    let config_path = dir.join("shunt.toml");
    std::fs::write(
        &config_path,
        format!(
            "[server]\ndefault_provider = \"antigravity\"\n\n\
             [providers.antigravity]\nauth = \"antigravity_oauth\"\nbase_url = \"{}\"\n",
            backend.uri()
        ),
    )
    .unwrap();
    let mut config = Config::load(Some(&config_path)).unwrap();
    config.server.bind = "127.0.0.1:0".to_string();

    let listener = tokio::net::TcpListener::bind(config.server.bind_addr().unwrap())
        .await
        .unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let (app, _, _) = server::build_router(config).unwrap();
    let gateway = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let response = reqwest::Client::new()
        .post(format!("http://{addr}/v1/messages"))
        .json(&json!({
            "model": "gemini-3.8-flash",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "Reply with OK."}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{:?}", response.text().await);

    let inference = backend
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .find(|request| request.url.path() == "/v1internal:generateContent")
        .expect("the gateway must have reached the inference endpoint");
    let body: Value = serde_json::from_slice(&inference.body).unwrap();

    // The catalog id goes out verbatim — not `gemini-3.8-flash-medium`, which
    // this account does not publish — and the tier it no longer names travels
    // in the request instead.
    assert_eq!(body["model"], "gemini-3.8-flash-tiered");
    assert_eq!(
        body["request"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "medium"
    );

    gateway.abort();
    std::env::remove_var("SHUNT_ANTIGRAVITY_AUTH_FILE");
    let _ = std::fs::remove_dir_all(&dir);
}
