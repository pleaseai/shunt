use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderName};
use serde_json::json;
use wiremock::{
    matchers::{header, method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

use crate::{
    auth::inbound::InboundAuth,
    config::{AccountConfig, ApiKeyHeader, AuthMode, ProviderKind},
    server::AppState,
};

use super::{anthropic_provider, fetch, InboundCredentialContext};

/// Point the default anthropic provider at `base_url` with the given auth.
fn config_for(base_url: &str, auth: AuthMode) -> crate::config::Config {
    let mut config = crate::config::Config::default();
    let provider = config.providers.get_mut("anthropic").unwrap();
    provider.base_url = base_url.to_string();
    provider.auth = auth;
    if auth == AuthMode::ApiKey {
        provider.api_key_env = Some("SHUNT_TEST_DISCOVERY_KEY".to_string());
        provider.api_key_header = ApiKeyHeader::XApiKey;
    } else if auth == AuthMode::ClaudeOauth {
        provider.accounts = vec![AccountConfig {
            name: "test-account".to_string(),
            token_env: Some("SHUNT_TEST_DISCOVERY_OAUTH_TOKEN".to_string()),
            ..AccountConfig::default()
        }];
    }
    config
}

fn page(models: serde_json::Value, has_more: bool, last_id: &str) -> serde_json::Value {
    json!({"data": models, "has_more": has_more, "first_id": null, "last_id": last_id})
}

fn passthrough_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", "caller-key".parse().unwrap());
    headers
}

async fn fetch_open(state: &AppState, inbound: &HeaderMap) -> Option<Vec<super::ModelEntry>> {
    fetch(state, inbound, InboundCredentialContext::default()).await
}

fn inbound_auth(token: &str) -> InboundAuth {
    InboundAuth::new(
        HeaderName::from_static("x-shunt-token"),
        vec![("test-client".to_string(), token.to_string())],
    )
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

    let models = fetch_open(&state, &headers).await.unwrap();

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

    assert!(fetch_open(&state, &HeaderMap::new()).await.is_none());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn consumed_x_api_key_is_not_forwarded_to_passthrough_upstream() {
    let server = MockServer::start().await;
    let state = AppState::new(
        config_for(&server.uri(), AuthMode::Passthrough),
        reqwest::Client::new(),
    )
    .unwrap();
    let auth = inbound_auth("gateway-token");
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", "gateway-token".parse().unwrap());

    let models = fetch(
        &state,
        &headers,
        InboundCredentialContext {
            static_auth: Some(&auth),
            gateway_bearer_authenticated: false,
        },
    )
    .await;

    assert!(models.is_none());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn upstream_x_api_key_survives_when_inbound_auth_is_also_configured() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "anthropic-key"))
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
    let auth = inbound_auth("gateway-token");
    let mut headers = HeaderMap::new();
    headers.insert("x-shunt-token", "gateway-token".parse().unwrap());
    headers.insert("x-api-key", "anthropic-key".parse().unwrap());

    let models = fetch(
        &state,
        &headers,
        InboundCredentialContext {
            static_auth: Some(&auth),
            gateway_bearer_authenticated: false,
        },
    )
    .await;

    assert_eq!(models.unwrap().len(), 1);
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

    assert!(fetch_open(&state, &headers).await.is_some());
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

    // Cargo runs tests concurrently, so serialize the process-wide env mutation
    // behind the shared lock, exactly as the `resolve_api_key` coverage in
    // src/auth/mod.rs does. SAFETY: the guard is held across the set/remove
    // window, so no other env-mutating test observes a partial state.
    let _guard = crate::auth::claude::store::TEST_ENV_LOCK.lock().await;
    unsafe { std::env::set_var("SHUNT_TEST_DISCOVERY_KEY", "env-key") };
    let state = AppState::new(
        config_for(&server.uri(), AuthMode::ApiKey),
        reqwest::Client::new(),
    )
    .unwrap();

    let models = fetch_open(&state, &HeaderMap::new()).await;
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

    let models = fetch_open(&state, &headers).await.unwrap();

    let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, ["claude-opus-5", "claude-sonnet-5"]);
}

#[tokio::test]
async fn overall_deadline_expires_before_a_slow_upstream_returns() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(query_param("limit", "1000"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(super::FETCH_TIMEOUT + Duration::from_secs(1))
                .set_body_json(page(
                    json!([{"type": "model", "id": "claude-opus-5"}]),
                    false,
                    "claude-opus-5",
                )),
        )
        .mount(&server)
        .await;

    let state = AppState::new(
        config_for(&server.uri(), AuthMode::Passthrough),
        reqwest::Client::new(),
    )
    .unwrap();
    let started = Instant::now();

    assert!(fetch_open(&state, &passthrough_headers()).await.is_none());
    let elapsed = started.elapsed();
    assert!(
        elapsed >= super::FETCH_TIMEOUT.saturating_sub(Duration::from_millis(250)),
        "fetch returned before the overall deadline: {elapsed:?}"
    );
    assert!(
        elapsed < super::FETCH_TIMEOUT + Duration::from_secs(1),
        "fetch waited for the delayed upstream response: {elapsed:?}"
    );
}

#[tokio::test]
async fn max_pages_backstop_falls_back_after_exactly_max_pages() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(|request: &wiremock::Request| {
            let page_number = request
                .url
                .query_pairs()
                .find_map(|(key, value)| (key == "after_id").then_some(value))
                .and_then(|cursor| cursor.strip_prefix("model-")?.parse::<usize>().ok())
                .map_or(1, |cursor| cursor + 1);
            let id = format!("model-{page_number}");
            ResponseTemplate::new(200).set_body_json(page(
                json!([{"type": "model", "id": id}]),
                true,
                &id,
            ))
        })
        .mount(&server)
        .await;

    let state = AppState::new(
        config_for(&server.uri(), AuthMode::Passthrough),
        reqwest::Client::new(),
    )
    .unwrap();

    assert!(fetch_open(&state, &passthrough_headers()).await.is_none());
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        super::MAX_PAGES
    );
}

#[tokio::test]
async fn has_more_without_usable_last_id_falls_back_after_the_first_page() {
    for last_id in [serde_json::Value::Null, json!("   ")] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"type": "model", "id": "claude-opus-5"}],
                "has_more": true,
                "first_id": "claude-opus-5",
                "last_id": last_id
            })))
            .mount(&server)
            .await;

        let state = AppState::new(
            config_for(&server.uri(), AuthMode::Passthrough),
            reqwest::Client::new(),
        )
        .unwrap();

        assert!(fetch_open(&state, &passthrough_headers()).await.is_none());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn claude_oauth_sends_bearer_and_beta_headers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer oauth-token"))
        .and(header("anthropic-beta", "oauth-2025-04-20"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            json!([{"type": "model", "id": "claude-opus-5"}]),
            false,
            "claude-opus-5",
        )))
        .mount(&server)
        .await;

    // Cargo runs tests concurrently, so serialize the process-wide env mutation
    // behind the shared lock, exactly as the `resolve_api_key` coverage in
    // src/auth/mod.rs does. SAFETY: the guard is held across the set/remove
    // window, so no other env-mutating test observes a partial state.
    let _guard = crate::auth::claude::store::TEST_ENV_LOCK.lock().await;
    unsafe { std::env::set_var("SHUNT_TEST_DISCOVERY_OAUTH_TOKEN", "oauth-token") };
    let state = AppState::new(
        config_for(&server.uri(), AuthMode::ClaudeOauth),
        reqwest::Client::new(),
    )
    .unwrap();

    let models = fetch_open(&state, &HeaderMap::new()).await;
    unsafe { std::env::remove_var("SHUNT_TEST_DISCOVERY_OAUTH_TOKEN") };

    assert_eq!(models.unwrap().len(), 1);
}

#[tokio::test]
async fn claude_oauth_resolves_account_scope_from_the_store() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer stored-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            json!([{"type": "model", "id": "claude-opus-5"}]),
            false,
            "claude-opus-5",
        )))
        .mount(&server)
        .await;

    let accounts_dir = std::env::temp_dir().join(format!(
        "shunt-discovery-store-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _guard = crate::auth::claude::store::TEST_ENV_LOCK.lock().await;
    let _env = crate::auth::shared::EnvVarGuard::set("SHUNT_CLAUDE_ACCOUNTS_DIR", &accounts_dir);
    crate::auth::claude::store::store_setup_token("stored", "stored-token", Some("stored-uuid"))
        .unwrap();
    let mut config = config_for(&server.uri(), AuthMode::ClaudeOauth);
    let provider = config.providers.get_mut("anthropic").unwrap();
    provider.accounts.clear();
    provider.account_scope = vec!["stored".to_string()];
    let state = AppState::new(config, reqwest::Client::new()).unwrap();

    let models = fetch_open(&state, &HeaderMap::new()).await;

    assert_eq!(models.unwrap().len(), 1);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    std::fs::remove_dir_all(accounts_dir).unwrap();
}

#[tokio::test]
async fn claude_oauth_skips_disabled_account() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer enabled-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            json!([{"type": "model", "id": "claude-opus-5"}]),
            false,
            "claude-opus-5",
        )))
        .mount(&server)
        .await;

    let mut config = config_for(&server.uri(), AuthMode::ClaudeOauth);
    config.providers.get_mut("anthropic").unwrap().accounts = vec![
        AccountConfig {
            name: "disabled".to_string(),
            token_env: Some("SHUNT_TEST_DISABLED_DISCOVERY_TOKEN".to_string()),
            disabled: true,
            ..AccountConfig::default()
        },
        AccountConfig {
            name: "enabled".to_string(),
            token_env: Some("SHUNT_TEST_ENABLED_DISCOVERY_TOKEN".to_string()),
            ..AccountConfig::default()
        },
    ];
    // Cargo runs tests concurrently, so serialize the process-wide env mutation
    // behind the shared lock, exactly as the `resolve_api_key` coverage in
    // src/auth/mod.rs does. SAFETY: the guard is held across the set/remove
    // window, so no other env-mutating test observes a partial state.
    let _guard = crate::auth::claude::store::TEST_ENV_LOCK.lock().await;
    unsafe {
        std::env::set_var("SHUNT_TEST_DISABLED_DISCOVERY_TOKEN", "disabled-token");
        std::env::set_var("SHUNT_TEST_ENABLED_DISCOVERY_TOKEN", "enabled-token");
    }
    let state = AppState::new(config, reqwest::Client::new()).unwrap();

    let models = fetch_open(&state, &HeaderMap::new()).await;
    unsafe {
        std::env::remove_var("SHUNT_TEST_DISABLED_DISCOVERY_TOKEN");
        std::env::remove_var("SHUNT_TEST_ENABLED_DISCOVERY_TOKEN");
    }

    assert_eq!(models.unwrap().len(), 1);
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

    assert!(fetch_open(&state, &headers).await.is_none());
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

    assert!(fetch_open(&state, &headers).await.is_none());
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
