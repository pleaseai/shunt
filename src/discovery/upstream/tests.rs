use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::json;
use wiremock::{
    matchers::{header, method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

use crate::{
    auth::inbound::{is_consumed_by_shunt, InboundAuth},
    config::{AccountConfig, AdminKey, AdminKeyring, ApiKeyHeader, AuthMode, ProviderKind, Secret},
    gateway::{approval::Identity, jwt, GatewayAuth},
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

fn single_model_page(id: &str) -> serde_json::Value {
    page(json!([{"type": "model", "id": id}]), false, id)
}

async fn mount_models_ok(server: &MockServer, body: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

async fn mount_models_ok_with_headers(
    server: &MockServer,
    headers: &[(&str, &str)],
    body: serde_json::Value,
) {
    let mut mock = Mock::given(method("GET")).and(path("/v1/models"));
    for &(name, value) in headers {
        mock = mock.and(header(name, value));
    }
    mock.respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

async fn mount_models_ok_after_id(server: &MockServer, after_id: &str, body: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(query_param("after_id", after_id))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

fn state_for(base_url: &str, auth: AuthMode) -> AppState {
    AppState::new(config_for(base_url, auth), reqwest::Client::new()).unwrap()
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

const GATEWAY_URL: &str = "https://gateway.example";
const GATEWAY_SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

/// A gateway auth verifying against the same issuer/secret `gateway_jwt` mints
/// with — not a fixture shaped like a JWT.
fn gateway_auth() -> GatewayAuth {
    GatewayAuth::with_optional_approval(
        GATEWAY_URL.to_string(),
        GATEWAY_SECRET.to_vec(),
        3600,
        false,
        None,
    )
}

fn gateway_jwt() -> String {
    jwt::mint(
        &Identity {
            sub: "dev".to_string(),
            email: "dev@example.com".to_string(),
            name: "Dev".to_string(),
        },
        GATEWAY_URL,
        GATEWAY_SECRET,
        3600,
    )
}

/// Start a mock server that answers `x-api-key: sk-ant-genuine-upstream-key`
/// with one model, paired with a passthrough `AppState` and `GatewayAuth` —
/// the shared fixture for the "gateway JWT in `Authorization` is stripped,
/// the distinct genuine `x-api-key` survives" tests below.
async fn passthrough_state_with_genuine_api_key_upstream() -> (MockServer, AppState, GatewayAuth) {
    let server = MockServer::start().await;
    mount_models_ok_with_headers(
        &server,
        &[("x-api-key", "sk-ant-genuine-upstream-key")],
        single_model_page("claude-opus-5"),
    )
    .await;
    let state = state_for(&server.uri(), AuthMode::Passthrough);
    let gateway = gateway_auth();
    (server, state, gateway)
}

/// Drive `fetch` with a gateway-auth context and assert the genuine
/// `x-api-key` reached the upstream while `authorization` was stripped.
async fn assert_genuine_api_key_forwarded_and_authorization_stripped(
    server: &MockServer,
    state: &AppState,
    gateway: &GatewayAuth,
    headers: HeaderMap,
) {
    let models = fetch(
        state,
        &headers,
        InboundCredentialContext {
            static_auth: None,
            gateway_auth: Some(gateway),
            admin_credentials: None,
        },
    )
    .await;

    assert_eq!(models.unwrap().len(), 1);
    let request = &server.received_requests().await.unwrap()[0];
    assert!(request.headers.get("authorization").is_none());
}

#[tokio::test]
async fn forwards_caller_credential_and_maps_every_field() {
    let server = MockServer::start().await;
    mount_models_ok_with_headers(
        &server,
        &[
            ("x-api-key", "caller-key"),
            ("anthropic-version", "2023-06-01"),
        ],
        page(
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
        ),
    )
    .await;

    let state = state_for(&server.uri(), AuthMode::Passthrough);
    let headers = passthrough_headers();

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
    let state = state_for(&server.uri(), AuthMode::Passthrough);

    assert!(fetch_open(&state, &HeaderMap::new()).await.is_none());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn consumed_x_api_key_is_not_forwarded_to_passthrough_upstream() {
    let server = MockServer::start().await;
    let state = state_for(&server.uri(), AuthMode::Passthrough);
    let auth = inbound_auth("gateway-token");
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", "gateway-token".parse().unwrap());

    let models = fetch(
        &state,
        &headers,
        InboundCredentialContext {
            static_auth: Some(&auth),
            gateway_auth: None,
            admin_credentials: None,
        },
    )
    .await;

    assert!(models.is_none());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_gateway_jwt_is_not_forwarded_in_the_api_key_slot() {
    // An `apiKeyHelper` sends its value in *both* `Authorization` and
    // `x-api-key` (Claude Code's `llm-gateway-connect` reference, "How the
    // credential variable maps to a header"), and it is the delivery mechanism
    // for any credential that rotates. So a gateway JWT does reach this slot,
    // and filtering it on static tokens alone strips the bearer while relaying
    // shunt's own identity token to a third party beside it.
    let server = MockServer::start().await;
    mount_models_ok(&server, single_model_page("claude-opus-5")).await;
    let state = state_for(&server.uri(), AuthMode::Passthrough);
    let auth = inbound_auth("gateway-token");
    let gateway = gateway_auth();
    let jwt = gateway_jwt();
    let mut headers = HeaderMap::new();
    headers.insert("authorization", format!("Bearer {jwt}").parse().unwrap());
    headers.insert("x-api-key", jwt.parse().unwrap());

    let models = fetch(
        &state,
        &headers,
        InboundCredentialContext {
            static_auth: Some(&auth),
            gateway_auth: Some(&gateway),
            admin_credentials: None,
        },
    )
    .await;

    // The mock is mounted, so a forwarded credential would have produced a
    // model; `upstream_x_api_key_survives_when_inbound_auth_is_also_configured`
    // is the non-vacuity control for an *unconsumed* `x-api-key`.
    assert!(models.is_none());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn upstream_x_api_key_survives_when_inbound_auth_is_also_configured() {
    let server = MockServer::start().await;
    mount_models_ok_with_headers(
        &server,
        &[("x-api-key", "anthropic-key")],
        single_model_page("claude-opus-5"),
    )
    .await;

    let state = state_for(&server.uri(), AuthMode::Passthrough);
    let auth = inbound_auth("gateway-token");
    let mut headers = HeaderMap::new();
    headers.insert("x-shunt-token", "gateway-token".parse().unwrap());
    headers.insert("x-api-key", "anthropic-key".parse().unwrap());

    let models = fetch(
        &state,
        &headers,
        InboundCredentialContext {
            static_auth: Some(&auth),
            gateway_auth: None,
            admin_credentials: None,
        },
    )
    .await;

    assert_eq!(models.unwrap().len(), 1);
}

#[tokio::test]
async fn a_gateway_jwt_in_authorization_does_not_block_a_distinct_api_key() {
    // Regression coverage for the per-slot fix: a gateway JWT in `Authorization`
    // alongside a genuine, *different* upstream key in `x-api-key` must forward
    // that key rather than treating the whole request as gateway-consumed.
    let (server, state, gateway) = passthrough_state_with_genuine_api_key_upstream().await;
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", gateway_jwt()).parse().unwrap(),
    );
    headers.insert("x-api-key", "sk-ant-genuine-upstream-key".parse().unwrap());

    assert_genuine_api_key_forwarded_and_authorization_stripped(&server, &state, &gateway, headers)
        .await;
}

#[tokio::test]
async fn a_gateway_jwt_present_only_in_the_api_key_slot_is_not_forwarded() {
    // Verification used to read only `authorization`, so a gateway JWT
    // arriving solely in `x-api-key` (no `authorization` at all) was relayed.
    let server = MockServer::start().await;
    // No mock is mounted: any request would fail the fetch anyway, but
    // `received_requests` proves none was even attempted.
    let state = state_for(&server.uri(), AuthMode::Passthrough);
    let gateway = gateway_auth();
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", gateway_jwt().parse().unwrap());

    let models = fetch(
        &state,
        &headers,
        InboundCredentialContext {
            static_auth: None,
            gateway_auth: Some(&gateway),
            admin_credentials: None,
        },
    )
    .await;

    assert!(models.is_none());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_static_token_configured_on_the_authorization_header_is_not_forwarded_raw() {
    // `[server.auth] header = "authorization"` is a valid configuration —
    // `InboundAuthConfig::resolve` only validates the name is a well-formed
    // `HeaderName` — and `InboundAuth::authenticate_client` authenticates such
    // a caller off the *whole* header value, with no `Bearer ` scheme
    // required. Verification that reads only the Bearer payload sees nothing
    // consumed for a raw token, so the gate token itself gets relayed to the
    // upstream: the same leak as the `Bearer` case, one slot shape over.
    let server = MockServer::start().await;
    // No mock is mounted: `received_requests` proves no relay was attempted.
    let state = state_for(&server.uri(), AuthMode::Passthrough);
    let auth = InboundAuth::new(
        HeaderName::from_static("authorization"),
        vec![("test-client".to_string(), "static-gate-token".to_string())],
    );
    let mut headers = HeaderMap::new();
    headers.insert("authorization", "static-gate-token".parse().unwrap());
    // The gate this caller actually passes, so the leak is reachable rather
    // than hypothetical.
    assert_eq!(auth.authenticate_client(&headers), Some("test-client"));

    let models = fetch(
        &state,
        &headers,
        InboundCredentialContext {
            static_auth: Some(&auth),
            gateway_auth: None,
            admin_credentials: None,
        },
    )
    .await;

    assert!(models.is_none());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[test]
fn non_utf8_value_is_not_consumed_by_shunt() {
    // `is_consumed_by_shunt`'s gateway-JWT branch has an explicit non-UTF-8
    // fallback (`std::str::from_utf8(value)`) with no dedicated coverage: a
    // header value that fails UTF-8 decoding can't be a JWT or a configured
    // static token, so it must be treated as the caller's own credential —
    // not shunt's — and kept for forwarding.
    let auth = gateway_auth();
    let non_utf8 = HeaderValue::from_bytes(&[0xff, 0xfe, b'x']).expect("opaque header value");

    assert!(!is_consumed_by_shunt(
        non_utf8.as_bytes(),
        Some(&auth),
        None,
        None
    ));
}

#[test]
fn garbage_three_segment_string_is_not_consumed_by_shunt() {
    // Malformed-input control: right segment count, no valid base64/JSON
    // payload — must not panic and must not be treated as shunt's own.
    let auth = gateway_auth();
    assert!(!is_consumed_by_shunt(b"a.b.c", Some(&auth), None, None));
}

/// A different secret than `gateway_auth()` verifies with — for minting a
/// well-formed, `aud = "shunt"` token that fails signature verification
/// (e.g. after a secret rotation) but is still shape-recognized.
const OTHER_SECRET: &[u8] = b"fedcba9876543210fedcba9876543210";

/// A different `public_url` than `GATEWAY_URL` — for minting a token as if by
/// a sibling instance sharing the same `jwt_secret` but configured under a
/// different issuer.
const SIBLING_URL: &str = "https://sibling.gateway.example";

fn identity_for_test() -> Identity {
    Identity {
        sub: "dev".to_string(),
        email: "dev@example.com".to_string(),
        name: "Dev".to_string(),
    }
}

#[tokio::test]
async fn expired_gateway_jwt_in_x_api_key_is_not_forwarded() {
    // #358 case 1: shunt itself minted this token (aud = "shunt", real
    // secret), but its ttl is 0, so it is already expired by the time
    // `verify_at` runs. It must still not reach the upstream.
    let server = MockServer::start().await;
    let state = state_for(&server.uri(), AuthMode::Passthrough);
    let gateway = gateway_auth();
    let expired = jwt::mint(&identity_for_test(), GATEWAY_URL, GATEWAY_SECRET, 0);
    assert!(gateway.authenticate_token(&expired).is_none());

    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", expired.parse().unwrap());

    let models = fetch(
        &state,
        &headers,
        InboundCredentialContext {
            static_auth: None,
            gateway_auth: Some(&gateway),
            admin_credentials: None,
        },
    )
    .await;

    assert!(models.is_none());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn wrong_issuer_gateway_jwt_in_authorization_is_not_forwarded() {
    // #358 case 2, the sharper one: a fleet sharing one `jwt_secret` across
    // differing `public_url` values. This token is minted for a sibling
    // instance, is still live, and fails verification here purely on issuer
    // mismatch — but `aud` is still "shunt", so shape still catches it. The
    // genuine upstream key in `x-api-key` must survive.
    let (server, state, gateway) = passthrough_state_with_genuine_api_key_upstream().await;
    let sibling_token = jwt::mint(&identity_for_test(), SIBLING_URL, GATEWAY_SECRET, 3600);
    assert!(gateway.authenticate_token(&sibling_token).is_none());

    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {sibling_token}").parse().unwrap(),
    );
    headers.insert("x-api-key", "sk-ant-genuine-upstream-key".parse().unwrap());

    assert_genuine_api_key_forwarded_and_authorization_stripped(&server, &state, &gateway, headers)
        .await;
}

#[tokio::test]
async fn bad_signature_gateway_jwt_is_not_forwarded() {
    // #358 case 3: minted under a different secret (e.g. post-rotation), so
    // verification rejects it on signature — but it is well-formed with
    // `aud = "shunt"`, so shape still catches it.
    let server = MockServer::start().await;
    let state = state_for(&server.uri(), AuthMode::Passthrough);
    let gateway = gateway_auth();
    let token = jwt::mint(&identity_for_test(), GATEWAY_URL, OTHER_SECRET, 3600);
    assert!(gateway.authenticate_token(&token).is_none());

    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", token.parse().unwrap());

    let models = fetch(
        &state,
        &headers,
        InboundCredentialContext {
            static_auth: None,
            gateway_auth: Some(&gateway),
            admin_credentials: None,
        },
    )
    .await;

    assert!(models.is_none());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn bare_authorization_header_with_an_invalid_shunt_jwt_is_not_forwarded() {
    // #358 case 4: `[server.auth] header = "authorization"` lets a bare
    // `Authorization: <token>` (no `Bearer ` scheme) pass the gate, so the
    // by-value check must also see the raw header value. Here the raw value
    // is an expired shunt-minted JWT.
    let (server, state, gateway) = passthrough_state_with_genuine_api_key_upstream().await;
    let expired = jwt::mint(&identity_for_test(), GATEWAY_URL, GATEWAY_SECRET, 0);

    let mut headers = HeaderMap::new();
    headers.insert("authorization", expired.parse().unwrap());
    headers.insert("x-api-key", "sk-ant-genuine-upstream-key".parse().unwrap());

    assert_genuine_api_key_forwarded_and_authorization_stripped(&server, &state, &gateway, headers)
        .await;
}

#[tokio::test]
async fn oauth_bearer_suppresses_duplicated_api_key() {
    let server = MockServer::start().await;
    mount_models_ok_with_headers(
        &server,
        &[("authorization", "Bearer sk-ant-oat-token")],
        single_model_page("claude-opus-5"),
    )
    .await;

    let state = state_for(&server.uri(), AuthMode::Passthrough);
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
    mount_models_ok_with_headers(
        &server,
        &[("x-api-key", "env-key")],
        single_model_page("claude-opus-5"),
    )
    .await;

    // Cargo runs tests concurrently, so serialize the process-wide env mutation
    // behind the shared lock, exactly as the `resolve_api_key` coverage in
    // src/auth/mod.rs does. SAFETY: the guard is held across the set/remove
    // window, so no other env-mutating test observes a partial state.
    let _guard = crate::auth::claude::store::TEST_ENV_LOCK.lock().await;
    unsafe { std::env::set_var("SHUNT_TEST_DISCOVERY_KEY", "env-key") };
    let state = state_for(&server.uri(), AuthMode::ApiKey);

    let models = fetch_open(&state, &HeaderMap::new()).await;
    unsafe { std::env::remove_var("SHUNT_TEST_DISCOVERY_KEY") };

    assert_eq!(models.unwrap().len(), 1);
}

#[tokio::test]
async fn follows_pagination_until_has_more_clears() {
    let server = MockServer::start().await;
    mount_models_ok_after_id(
        &server,
        "claude-opus-5",
        single_model_page("claude-sonnet-5"),
    )
    .await;
    mount_models_ok(
        &server,
        page(
            json!([{"type": "model", "id": "claude-opus-5"}]),
            true,
            "claude-opus-5",
        ),
    )
    .await;

    let state = state_for(&server.uri(), AuthMode::Passthrough);
    let headers = passthrough_headers();

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

    let state = state_for(&server.uri(), AuthMode::Passthrough);
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

    let state = state_for(&server.uri(), AuthMode::Passthrough);

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

        let state = state_for(&server.uri(), AuthMode::Passthrough);

        assert!(fetch_open(&state, &passthrough_headers()).await.is_none());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn claude_oauth_sends_bearer_and_beta_headers() {
    let server = MockServer::start().await;
    mount_models_ok_with_headers(
        &server,
        &[
            ("authorization", "Bearer oauth-token"),
            ("anthropic-beta", "oauth-2025-04-20"),
        ],
        single_model_page("claude-opus-5"),
    )
    .await;

    // Cargo runs tests concurrently, so serialize the process-wide env mutation
    // behind the shared lock, exactly as the `resolve_api_key` coverage in
    // src/auth/mod.rs does. SAFETY: the guard is held across the set/remove
    // window, so no other env-mutating test observes a partial state.
    let _guard = crate::auth::claude::store::TEST_ENV_LOCK.lock().await;
    unsafe { std::env::set_var("SHUNT_TEST_DISCOVERY_OAUTH_TOKEN", "oauth-token") };
    let state = state_for(&server.uri(), AuthMode::ClaudeOauth);

    let models = fetch_open(&state, &HeaderMap::new()).await;
    unsafe { std::env::remove_var("SHUNT_TEST_DISCOVERY_OAUTH_TOKEN") };

    assert_eq!(models.unwrap().len(), 1);
}

#[tokio::test]
async fn claude_oauth_resolves_account_scope_from_the_store() {
    let server = MockServer::start().await;
    mount_models_ok_with_headers(
        &server,
        &[("authorization", "Bearer stored-token")],
        single_model_page("claude-opus-5"),
    )
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
    mount_models_ok_with_headers(
        &server,
        &[("authorization", "Bearer enabled-token")],
        single_model_page("claude-opus-5"),
    )
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

    let state = state_for(&server.uri(), AuthMode::Passthrough);
    let headers = passthrough_headers();

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

    let state = state_for(&server.uri(), AuthMode::Passthrough);
    let headers = passthrough_headers();

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
fn non_anthropic_default_provider_skips_other_anthropic_upstreams() {
    let mut config = crate::config::Config::default();
    config.server.default_provider = "codex".to_string();

    // Unmatched ids route to the default provider, so discovery must not
    // advertise ids from a different Anthropic-kind upstream.
    assert!(anthropic_provider(&config).is_none());
}

#[test]
fn anthropic_default_provider_is_used() {
    let config = crate::config::Config::default();
    let (name, _) = anthropic_provider(&config).unwrap();

    assert_eq!(name, config.server.default_provider);
}

// --- Admin credentials (#346) ------------------------------------------------

const ADMIN_WRITE_KEY: &str = "admin-write-key-0123456789abcdef0";

/// A resolved admin keyring holding one credential of each kind, so the
/// discovery-path predicate is exercised against the same three sets the
/// admin/spend routers authenticate against.
fn admin_keyring() -> AdminKeyring {
    AdminKeyring::new(
        &[(
            "ops".to_string(),
            "admin-legacy-token-0123456789abcd".to_string(),
        )],
        &[AdminKey {
            id: "writer".to_string(),
            key: Secret::from(ADMIN_WRITE_KEY),
        }],
        &[AdminKey {
            id: "reader".to_string(),
            key: Secret::from("admin-read-key-0123456789abcdef01"),
        }],
    )
}

#[tokio::test]
async fn an_admin_credential_is_not_forwarded_in_the_api_key_slot() {
    // `AdminAuth::authenticate_credential` accepts `x-api-key`, so the
    // discovery path — which relays the caller's headers to the passthrough
    // upstream — has to strip it there too, or an admin key that can provision
    // upstream accounts is handed to the provider verbatim.
    let server = MockServer::start().await;
    mount_models_ok(&server, single_model_page("claude-opus-5")).await;
    let state = state_for(&server.uri(), AuthMode::Passthrough);
    let keyring = admin_keyring();
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", ADMIN_WRITE_KEY.parse().unwrap());

    let models = fetch(
        &state,
        &headers,
        InboundCredentialContext {
            static_auth: None,
            gateway_auth: None,
            admin_credentials: Some(&keyring),
        },
    )
    .await;

    // The mock is mounted, so a forwarded credential would have produced a
    // model; the control below proves an unrelated value still reaches it.
    assert!(models.is_none());
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_non_admin_api_key_survives_when_admin_credentials_are_configured() {
    // Non-vacuity control: configuring `[server.admin]` must not itself strip
    // the slot — only a value that is actually one of the admin credentials.
    let server = MockServer::start().await;
    mount_models_ok_with_headers(
        &server,
        &[("x-api-key", "sk-ant-genuine-upstream-key")],
        single_model_page("claude-opus-5"),
    )
    .await;
    let state = state_for(&server.uri(), AuthMode::Passthrough);
    let keyring = admin_keyring();
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", "sk-ant-genuine-upstream-key".parse().unwrap());

    let models = fetch(
        &state,
        &headers,
        InboundCredentialContext {
            static_auth: None,
            gateway_auth: None,
            admin_credentials: Some(&keyring),
        },
    )
    .await;

    assert_eq!(models.unwrap().len(), 1);
}
