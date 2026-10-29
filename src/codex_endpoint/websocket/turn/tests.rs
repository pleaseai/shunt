#![allow(clippy::await_holding_lock)] // Intentional cross-module test serialization.

use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use axum::body::Bytes;
use axum::http::HeaderMap;
use serde_json::json;
use tokio::sync::mpsc;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use super::{run_turn, upstream_error_parts, TurnContext};
use crate::client_attribution::{ClientAttribution, ClientKind};
use crate::codex_endpoint::websocket::continuation::TurnRecorder;
use crate::config::{CodexEndpointConfig, CodexRouteConfig, Config, InboundAuthConfig};
use crate::server::AppState;

#[test]
fn upstream_detail_shape_is_surfaced_not_masked() {
    // The ChatGPT HTTP backend's actual rejection for a forwarded
    // `previous_response_id`; masking it hid the cause for a day.
    let body = json!({"detail": "Unsupported parameter: previous_response_id"});
    let (_, _, message) = upstream_error_parts(&body);
    assert_eq!(message, "Unsupported parameter: previous_response_id");
}

#[test]
fn openai_error_shape_still_reads_nested_fields() {
    let body =
        json!({"error": {"type": "invalid_request_error", "code": "bad", "message": "nope"}});
    assert_eq!(
        upstream_error_parts(&body),
        (
            "invalid_request_error".to_string(),
            "bad".to_string(),
            "nope".to_string()
        )
    );
}

/// A WebSocket turn is attributed from the client's headers *before* the
/// `[server.auth]` header is stripped. An operator may name that header
/// `originator` — the very marker `classify_client` reads as Codex — so the
/// token-bearing header is also the turn's only client marker here (no
/// `user-agent`). Moving `inbound_attribution(&headers)` below the strip block
/// in `run_turn` erases the marker and records the turn as `client_kind =
/// "other"`, turning this red (`docs/otlp-client-attribution.md`, issue #813).
///
/// The upstream assertion guards the outcome only, not `run_turn`'s strip:
/// `forward_codex_routed` sends a fresh header allowlist that never carries
/// `originator`, so deleting every strip in `run_turn` leaves this green. The
/// strip `run_turn` alone performs is pinned by
/// [`ws_turn_strips_the_upgrade_time_auth_header_a_reload_renamed`].
#[tokio::test]
async fn ws_turn_attributes_before_stripping_an_auth_header_named_like_a_client_marker() {
    const MODEL: &str = "ws-attribution-813-model";
    const TOKEN: &str = "ws-813-client-token";

    let upstream = MockServer::start().await;
    let addr: SocketAddr = *upstream.address();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_813\"}}\n\n",
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;

    let _lock = crate::config::CONFIG_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _key = crate::auth::shared::EnvVarGuard::set("SHUNT_TEST_813_OPENAI_KEY", "openai-key");
    let _tokens = crate::auth::shared::EnvVarGuard::set(
        "SHUNT_TEST_813_CLIENT_TOKENS",
        format!("alice:{TOKEN}"),
    );

    let mut config = Config::default();
    let openai = config.providers.get_mut("openai").unwrap();
    openai.base_url = "http://api.openai.com/v1".to_string();
    openai.api_key_env = Some("SHUNT_TEST_813_OPENAI_KEY".to_string());
    config.server.auth = Some(InboundAuthConfig {
        header: "originator".to_string(),
        tokens_env: "SHUNT_TEST_813_CLIENT_TOKENS".to_string(),
        jwt: Vec::new(),
        ..Default::default()
    });
    config.server.codex_endpoint = Some(CodexEndpointConfig {
        provider: "codex".to_string(),
        routes: vec![CodexRouteConfig {
            model: MODEL.to_string(),
            provider: "openai".to_string(),
            upstream_model: None,
        }],
    });
    let http_client = reqwest::Client::builder()
        .resolve("api.openai.com", addr)
        .build()
        .unwrap();
    let state = AppState::new(config, http_client).unwrap();
    let auth_header = state
        .inbound_auth
        .as_ref()
        .map(|auth| auth.header().clone());
    assert_eq!(auth_header.as_ref().map(|h| h.as_str()), Some("originator"));

    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());
    headers.insert("originator", TOKEN.parse().unwrap());

    let (out_tx, mut out_rx) = mpsc::channel(8);
    let recorder = TurnRecorder::new(
        Arc::new(std::sync::Mutex::new(None)),
        "openai".to_string(),
        Vec::new(),
        String::new(),
    );
    run_turn(TurnContext {
        state,
        model: Some(MODEL.to_string()),
        session_id: None,
        headers,
        auth_header,
        jwt_header: None,
        admin_header: None,
        body: Bytes::from(format!(r#"{{"model":"{MODEL}","stream":true,"input":[]}}"#)),
        estimate_body: None,
        generation: 1,
        current_generation: Arc::new(AtomicU64::new(1)),
        out_tx,
        recorder,
    })
    .await;

    // The turn authenticated and relayed the upstream terminal event, rather
    // than ending on a gateway error frame.
    let (_, frame) = out_rx.recv().await.expect("the turn sent no frame");
    let axum::extract::ws::Message::Text(frame) = frame else {
        panic!("expected a text frame, got {frame:?}");
    };
    assert!(
        frame.as_str().contains("response.completed"),
        "unexpected frame: {}",
        frame.as_str()
    );

    let codex = ClientAttribution {
        client_kind: ClientKind::Codex,
        managed: false,
    };
    let (count, _) = crate::metrics::proxied_request_samples_by_attribution_for_tests(
        "client", codex, "openai", MODEL, 200,
    );
    let other = ClientAttribution {
        client_kind: ClientKind::Other,
        managed: false,
    };
    let (other_count, _) = crate::metrics::proxied_request_samples_by_attribution_for_tests(
        "client", other, "openai", MODEL, 200,
    );
    assert_eq!(
        (count, other_count),
        (1, 0),
        "the turn must be counted as client_kind=codex (got {count} codex, {other_count} other)"
    );

    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
        !requests[0].headers.contains_key("originator"),
        "the [server.auth] header reached the upstream"
    );
    upstream.verify().await;
}

/// Removes the temp accounts dir the test pins `SHUNT_CODEX_ACCOUNTS_DIR` at.
struct EmptyAccountsDir(std::path::PathBuf);

impl Drop for EmptyAccountsDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A WebSocket turn strips the `[server.auth]` header name captured at
/// upgrade (`TurnContext.auth_header`), not only the refreshed config's. A
/// reload can rename the header mid-connection; the pool passthrough
/// (`forward_codex_inbound`) forwards every client header and its own
/// `strip_reserved_slots` knows only the *refreshed* name, so the stale
/// upgrade-time header would reach the ChatGPT backend with its token unless
/// `run_turn` removes it. Deleting `run_turn`'s `auth_header` strip turns this
/// red.
///
/// Not pinned here: `run_turn`'s strips of the refreshed `state.inbound_auth`
/// and `state.admin_auth` headers. Both downstream paths remove those again
/// (the routed allowlist, and `strip_reserved_slots` on the passthrough), so
/// no upstream observation can tell whether `run_turn` did. The turn is still
/// attributed from `originator` before any strip (issue #813).
#[tokio::test]
async fn ws_turn_strips_the_upgrade_time_auth_header_a_reload_renamed() {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

    const MODEL: &str = "ws-passthrough-813-model";
    const TOKEN: &str = "ws-813-passthrough-token";

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_813p\"}}\n\n",
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;

    let _lock = crate::config::CONFIG_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let accounts_dir = EmptyAccountsDir(std::env::temp_dir().join(format!(
        "shunt-813-accounts-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )));
    std::fs::create_dir_all(&accounts_dir.0).unwrap();
    let _accounts_env =
        crate::auth::shared::EnvVarGuard::set("SHUNT_CODEX_ACCOUNTS_DIR", &accounts_dir.0);
    let claims = json!({
        "exp": 4_000_000_000u64,
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct_813"}
    });
    let account_token = format!(
        "x.{}.y",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    );
    let _account_env =
        crate::auth::shared::EnvVarGuard::set("SHUNT_TEST_813_CODEX_TOKEN", account_token);
    let _tokens = crate::auth::shared::EnvVarGuard::set(
        "SHUNT_TEST_813_PASSTHROUGH_TOKENS",
        format!("alice:{TOKEN}"),
    );

    let mut config = Config::default();
    let codex = config.providers.get_mut("codex").unwrap();
    codex.base_url = upstream.uri();
    codex.accounts = vec![crate::config::AccountConfig {
        name: "acct-813".to_string(),
        token_env: Some("SHUNT_TEST_813_CODEX_TOKEN".to_string()),
        ..Default::default()
    }];
    config.server.auth = Some(InboundAuthConfig {
        header: "originator".to_string(),
        tokens_env: "SHUNT_TEST_813_PASSTHROUGH_TOKENS".to_string(),
        jwt: Vec::new(),
        ..Default::default()
    });
    config.server.codex_endpoint = Some(CodexEndpointConfig {
        provider: "codex".to_string(),
        routes: Vec::new(),
    });
    let state = AppState::new(config, reqwest::Client::new()).unwrap();

    // The refreshed config names `originator`; the connection upgraded while
    // the header was still `x-upgrade-auth`, and the client keeps sending it.
    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());
    headers.insert("originator", TOKEN.parse().unwrap());
    headers.insert("x-upgrade-auth", "stale-upgrade-token".parse().unwrap());
    headers.insert("x-813-probe", "forwarded".parse().unwrap());

    let (out_tx, mut out_rx) = mpsc::channel(8);
    let recorder = TurnRecorder::new(
        Arc::new(std::sync::Mutex::new(None)),
        "codex".to_string(),
        Vec::new(),
        String::new(),
    );
    run_turn(TurnContext {
        state,
        model: Some(MODEL.to_string()),
        session_id: None,
        headers,
        auth_header: Some(axum::http::HeaderName::from_static("x-upgrade-auth")),
        jwt_header: None,
        admin_header: None,
        body: Bytes::from(format!(r#"{{"model":"{MODEL}","stream":true,"input":[]}}"#)),
        estimate_body: None,
        generation: 1,
        current_generation: Arc::new(AtomicU64::new(1)),
        out_tx,
        recorder,
    })
    .await;

    let (_, frame) = out_rx.recv().await.expect("the turn sent no frame");
    let axum::extract::ws::Message::Text(frame) = frame else {
        panic!("expected a text frame, got {frame:?}");
    };
    assert!(
        frame.as_str().contains("response.completed"),
        "unexpected frame: {}",
        frame.as_str()
    );

    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent = &requests[0].headers;
    // The passthrough relays an arbitrary client header, so the absences
    // below are the strips' doing, not an allowlist's.
    assert_eq!(
        sent.get("x-813-probe").and_then(|v| v.to_str().ok()),
        Some("forwarded")
    );
    assert!(
        !sent.contains_key("x-upgrade-auth"),
        "the upgrade-time [server.auth] header reached the upstream"
    );
    assert!(
        !sent.contains_key("originator"),
        "the [server.auth] header reached the upstream"
    );

    let codex = ClientAttribution {
        client_kind: ClientKind::Codex,
        managed: false,
    };
    let (count, _) = crate::metrics::proxied_request_samples_by_attribution_for_tests(
        "client", codex, "codex", MODEL, 200,
    );
    assert_eq!(count, 1, "the turn must be counted as client_kind=codex");
    upstream.verify().await;
}

/// Drives one passthrough turn that upgraded while `[server.auth] jwt_header`
/// was `x-upgrade-jwt` (`TurnContext.jwt_header`), against a refreshed state
/// whose `jwt_header` is `refreshed` (`None` models a reload that removed it),
/// and returns the headers the upstream received. `tag` only names the temp dir
/// and model.
async fn upstream_headers_after_jwt_header_reload(
    tag: &str,
    refreshed: Option<&'static str>,
) -> HeaderMap {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

    let model = format!("ws-{tag}-model");
    let token = format!("ws-{tag}-passthrough-token");
    // Fixed names: `CONFIG_ENV_LOCK` below already serializes these tests.
    const ACCOUNT_ENV: &str = "SHUNT_TEST_861_CODEX_TOKEN";
    const TOKENS_ENV: &str = "SHUNT_TEST_861_PASSTHROUGH_TOKENS";

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_jwt\"}}\n\n",
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;

    let _lock = crate::config::CONFIG_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let accounts_dir = EmptyAccountsDir(std::env::temp_dir().join(format!(
        "shunt-{tag}-accounts-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )));
    std::fs::create_dir_all(&accounts_dir.0).unwrap();
    let _accounts_env =
        crate::auth::shared::EnvVarGuard::set("SHUNT_CODEX_ACCOUNTS_DIR", &accounts_dir.0);
    let claims = json!({
        "exp": 4_000_000_000u64,
        "https://api.openai.com/auth": {"chatgpt_account_id": "acct_jwt"}
    });
    let account_token = format!(
        "x.{}.y",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    );
    let _account_env = crate::auth::shared::EnvVarGuard::set(ACCOUNT_ENV, account_token);
    let _tokens = crate::auth::shared::EnvVarGuard::set(TOKENS_ENV, format!("alice:{token}"));

    let mut config = Config::default();
    let codex = config.providers.get_mut("codex").unwrap();
    codex.base_url = upstream.uri();
    codex.accounts = vec![crate::config::AccountConfig {
        name: format!("acct-{tag}"),
        token_env: Some(ACCOUNT_ENV.to_string()),
        ..Default::default()
    }];
    config.server.auth = Some(InboundAuthConfig {
        header: "originator".to_string(),
        tokens_env: TOKENS_ENV.to_string(),
        ..Default::default()
    });
    config.server.codex_endpoint = Some(CodexEndpointConfig {
        provider: "codex".to_string(),
        routes: Vec::new(),
    });
    let mut state = AppState::new(config, reqwest::Client::new()).unwrap();
    // `jwt_header` needs a `[[server.auth.jwt]]` issuer to resolve through
    // config, which plays no part in stripping, so the refreshed slot is
    // attached directly.
    let auth = state.inbound_auth.as_deref().unwrap().clone();
    state.inbound_auth = Some(Arc::new(auth.with_jwt_slots(
        crate::auth::jwt_slots::JwtSlots {
            header: refreshed.map(axum::http::HeaderName::from_static),
            cookie: None,
        },
    )));

    let mut headers = HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());
    headers.insert("originator", token.parse().unwrap());
    headers.insert("x-upgrade-jwt", "stale-upgrade-jwt".parse().unwrap());
    headers.insert("x-jwt-probe", "forwarded".parse().unwrap());

    let (out_tx, mut out_rx) = mpsc::channel(8);
    let recorder = TurnRecorder::new(
        Arc::new(std::sync::Mutex::new(None)),
        "codex".to_string(),
        Vec::new(),
        String::new(),
    );
    run_turn(TurnContext {
        state,
        model: Some(model.clone()),
        session_id: None,
        headers,
        auth_header: None,
        jwt_header: Some(axum::http::HeaderName::from_static("x-upgrade-jwt")),
        admin_header: None,
        body: Bytes::from(format!(r#"{{"model":"{model}","stream":true,"input":[]}}"#)),
        estimate_body: None,
        generation: 1,
        current_generation: Arc::new(AtomicU64::new(1)),
        out_tx,
        recorder,
    })
    .await;

    let (_, frame) = out_rx.recv().await.expect("the turn sent no frame");
    let axum::extract::ws::Message::Text(frame) = frame else {
        panic!("expected a text frame, got {frame:?}");
    };
    assert!(
        frame.as_str().contains("response.completed"),
        "unexpected frame: {}",
        frame.as_str()
    );
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent = requests[0].headers.clone();
    upstream.verify().await;
    let mut out = HeaderMap::new();
    for (name, value) in sent.iter() {
        out.append(
            axum::http::HeaderName::from_bytes(name.as_str().as_bytes()).unwrap(),
            axum::http::HeaderValue::from_bytes(value.as_bytes()).unwrap(),
        );
    }
    out
}

/// A turn strips the `jwt_header` captured at upgrade (`TurnContext.jwt_header`)
/// when a reload renamed it: the passthrough forwards every client header and
/// its own `strip_reserved_slots` knows only the refreshed name. Deleting
/// `run_turn`'s `jwt_header` strip turns this and the removal test red.
#[tokio::test]
async fn ws_turn_strips_the_upgrade_time_jwt_header_a_reload_renamed() {
    let sent = upstream_headers_after_jwt_header_reload("861a", Some("x-renamed-jwt")).await;
    assert_eq!(
        sent.get("x-jwt-probe").and_then(|v| v.to_str().ok()),
        Some("forwarded"),
        "the passthrough must still relay arbitrary client headers"
    );
    assert!(
        !sent.contains_key("x-upgrade-jwt"),
        "the upgrade-time jwt_header reached the upstream"
    );
}

/// Same, for a reload that removed `jwt_header` altogether: the refreshed
/// state names nothing, so only the upgrade-time name can strip it.
#[tokio::test]
async fn ws_turn_strips_the_upgrade_time_jwt_header_a_reload_removed() {
    let sent = upstream_headers_after_jwt_header_reload("861b", None).await;
    assert_eq!(
        sent.get("x-jwt-probe").and_then(|v| v.to_str().ok()),
        Some("forwarded"),
        "the passthrough must still relay arbitrary client headers"
    );
    assert!(
        !sent.contains_key("x-upgrade-jwt"),
        "the upgrade-time jwt_header reached the upstream"
    );
}
