use axum::http::HeaderMap;
use serde_json::json;
use wiremock::{
    matchers::{header, method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

use crate::{
    config::{ApiKeyHeader, AuthMode, ProviderKind},
    server::AppState,
};

use super::{anthropic_provider, fetch};

/// Point the default anthropic provider at `base_url` with the given auth.
fn config_for(base_url: &str, auth: AuthMode) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    let provider = config.providers.get_mut("anthropic").unwrap();
    provider.base_url = base_url.to_string();
    provider.auth = auth;
    if auth == AuthMode::ApiKey {
        provider.api_key_env = Some("SHUNT_TEST_DISCOVERY_KEY".to_string());
        provider.api_key_header = ApiKeyHeader::XApiKey;
    }
    config
}

fn page(models: serde_json::Value, has_more: bool, last_id: &str) -> serde_json::Value {
    json!({"data": models, "has_more": has_more, "first_id": null, "last_id": last_id})
}

#[tokio::test]
async fn forwards_caller_credential_and_maps_every_field() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "caller-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            json!([{
                "type": "model",
                "id": "claude-opus-5",
                "display_name": "Claude Opus 5",
                "created_at": "2026-07-24T00:00:00Z",
                "max_input_tokens": 1_000_000,
                "max_tokens": 128_000,
                "capabilities": {"effort": {"supported": true}}
            }]),
            false,
            "claude-opus-5",
        )))
        .mount(&server)
        .await;

    let state = AppState::new(
        config_for(&server.uri(), AuthMode::Passthrough),
        reqwest::Client::new(),
    )
    .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", "caller-key".parse().unwrap());

    let models = fetch(&state, &headers).await.unwrap();

    assert_eq!(models.len(), 1);
    let body = serde_json::to_value(&models[0]).unwrap();
    assert_eq!(
        body,
        json!({
            "type": "model",
            "id": "claude-opus-5",
            "display_name": "Claude Opus 5",
            "created_at": "2026-07-24T00:00:00Z",
            "max_input_tokens": 1_000_000,
            "max_tokens": 128_000,
            "capabilities": {"effort": {"supported": true}}
        })
    );
}

#[tokio::test]
async fn passthrough_without_caller_credential_does_not_call_upstream() {
    let server = MockServer::start().await;
    // No mock is mounted: any request would 404 and fail the fetch anyway,
    // but `received_requests` proves none was even attempted.
    let state = AppState::new(
        config_for(&server.uri(), AuthMode::Passthrough),
        reqwest::Client::new(),
    )
    .unwrap();

    assert!(fetch(&state, &HeaderMap::new()).await.is_none());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn oauth_bearer_suppresses_duplicated_api_key() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer sk-ant-oat-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            json!([{"type": "model", "id": "claude-opus-5"}]),
            false,
            "claude-opus-5",
        )))
        .mount(&server)
        .await;

    let state = AppState::new(
        config_for(&server.uri(), AuthMode::Passthrough),
        reqwest::Client::new(),
    )
    .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("authorization", "Bearer sk-ant-oat-token".parse().unwrap());
    headers.insert("x-api-key", "sk-ant-oat-token".parse().unwrap());

    assert!(fetch(&state, &headers).await.is_some());
    let request = &server.received_requests().await.unwrap()[0];
    assert!(request.headers.get("x-api-key").is_none());
}

#[tokio::test]
async fn injects_configured_api_key_without_a_caller_credential() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "env-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            json!([{"type": "model", "id": "claude-opus-5"}]),
            false,
            "claude-opus-5",
        )))
        .mount(&server)
        .await;

    // SAFETY: single-threaded test process env mutation, mirroring the
    // existing `resolve_api_key` coverage in src/auth/mod.rs.
    unsafe { std::env::set_var("SHUNT_TEST_DISCOVERY_KEY", "env-key") };
    let state = AppState::new(
        config_for(&server.uri(), AuthMode::ApiKey),
        reqwest::Client::new(),
    )
    .unwrap();

    let models = fetch(&state, &HeaderMap::new()).await;
    unsafe { std::env::remove_var("SHUNT_TEST_DISCOVERY_KEY") };

    assert_eq!(models.unwrap().len(), 1);
}

#[tokio::test]
async fn follows_pagination_until_has_more_clears() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(query_param("after_id", "claude-opus-5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            json!([{"type": "model", "id": "claude-sonnet-5"}]),
            false,
            "claude-sonnet-5",
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            json!([{"type": "model", "id": "claude-opus-5"}]),
            true,
            "claude-opus-5",
        )))
        .mount(&server)
        .await;

    let state = AppState::new(
        config_for(&server.uri(), AuthMode::Passthrough),
        reqwest::Client::new(),
    )
    .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", "caller-key".parse().unwrap());

    let models = fetch(&state, &headers).await.unwrap();

    let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, ["claude-opus-5", "claude-sonnet-5"]);
}

#[tokio::test]
async fn upstream_error_yields_none_so_discovery_falls_back() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let state = AppState::new(
        config_for(&server.uri(), AuthMode::Passthrough),
        reqwest::Client::new(),
    )
    .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", "caller-key".parse().unwrap());

    assert!(fetch(&state, &headers).await.is_none());
}

#[tokio::test]
async fn empty_upstream_list_falls_back_rather_than_emptying_discovery() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(json!([]), false, "")))
        .mount(&server)
        .await;

    let state = AppState::new(
        config_for(&server.uri(), AuthMode::Passthrough),
        reqwest::Client::new(),
    )
    .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", "caller-key".parse().unwrap());

    assert!(fetch(&state, &headers).await.is_none());
}

#[test]
fn non_anthropic_upstream_is_skipped() {
    let mut config = crate::config::Config::default();
    config.providers.get_mut("anthropic").unwrap().kind = ProviderKind::Responses;
    config
        .providers
        .retain(|_, provider| provider.kind != ProviderKind::Anthropic);

    assert!(anthropic_provider(&config).is_none());
}

#[test]
fn default_provider_wins_over_declaration_order() {
    let config = crate::config::Config::default();
    let (name, _) = anthropic_provider(&config).unwrap();

    assert_eq!(name, config.server.default_provider);
}
