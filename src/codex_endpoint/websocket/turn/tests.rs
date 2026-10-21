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
/// The upstream assertion is the positive twin: the credential header never
/// reaches the routed provider. It is not specific to `run_turn`'s own strip:
/// `forward_codex_routed` sends a fresh header allowlist that never carries
/// `originator`, and the pool passthrough strips the configured header again
/// (`ShuntCredentials::strip_reserved_slots`), so deleting that strip leaves
/// this green — it guards the outcome, not that line.
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
        admin_header: None,
        body: Bytes::from(format!(r#"{{"model":"{MODEL}","stream":true,"input":[]}}"#)),
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
