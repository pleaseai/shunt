use std::sync::Arc;

use axum::{body::to_bytes, http::StatusCode};
use serde_json::{json, Value};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use super::*;
use crate::adapters::responses::{
    body::prepare_body,
    context::{ForwardOptions, TurnOptions},
    forward_single,
};
use crate::{config::Config, routing};

fn state(base_url: &str, websocket: bool) -> AppState {
    let mut config = Config::default();
    config.server.default_provider = "codex".into();
    let provider = config.providers.get_mut("codex").unwrap();
    provider.base_url = base_url.into();
    provider.websocket = websocket;
    provider.request_compression = false;
    AppState::new(config, reqwest::Client::new()).unwrap()
}

fn credential(account_id: &str) -> Credential {
    Credential::ChatGptOAuth {
        access_token: "synthetic-token".into(),
        account_id: account_id.into(),
    }
}

fn options(credential: Credential) -> ForwardOptions {
    ForwardOptions {
        upstream_body: Arc::new(json!({"model": "gpt-6-astra", "input": []})),
        credential,
        auth: crate::config::AuthMode::ChatgptOauth,
        turn: TurnOptions {
            client_wants_stream: false,
            thinking_enabled: false,
            tool_search_native: false,
        },
        codex_quota_account: None,
        estimate_input: None,
    }
}

#[tokio::test]
async fn common_boundary_rejects_local_headers_and_urls_without_transport() {
    let server = MockServer::start().await;
    for (base_url, credential, session_id) in [
        (server.uri(), credential("invalid\naccount"), None),
        (
            server.uri(),
            Credential::ApiKey {
                value: "invalid\nbearer".into(),
                header: crate::config::ApiKeyHeader::Bearer,
            },
            None,
        ),
        (
            server.uri(),
            credential("valid-account"),
            Some("invalid\nsession"),
        ),
        ("not a URL".into(), credential("valid-account"), None),
        ("ftp://127.0.0.1".into(), credential("valid-account"), None),
        (
            "https://{{hostname}}".into(),
            credential("valid-account"),
            None,
        ),
    ] {
        let mut state = state(&server.uri(), false);
        Arc::make_mut(&mut state.config)
            .providers
            .get_mut("codex")
            .unwrap()
            .base_url = base_url;
        let route = routing::resolve_model(&state.config, "gpt-6-astra");
        let body = prepare_body(&state, &route, &json!({"input": []})).await;
        let error = http_send(&state, &route, credential, session_id, body)
            .await
            .unwrap_err();

        assert!(!error.is_transient());
        let mapped = send_error(error);
        assert_eq!(mapped.failure, Some(AdapterFailure::NoUpstreamAttempt));
        assert_eq!(mapped.response.status(), StatusCode::BAD_GATEWAY);
        let body = to_bytes(mapped.response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["type"], "error");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn redirect_builder_failure_retains_transport_provenance() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", "ftp://127.0.0.1/next"))
        .expect(1)
        .mount(&server)
        .await;
    let state = state(&server.uri(), false);
    let route = routing::resolve_model(&state.config, "gpt-6-astra");
    let body = prepare_body(&state, &route, &json!({"input": []})).await;
    let error = http_send(&state, &route, credential("valid-account"), None, body)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, SendError::Transport(RequestError::Transport(error)) if error.is_builder())
    );
    assert_eq!(
        send_error(error).failure,
        Some(AdapterFailure::BeforeHeaders)
    );
    server.verify().await;
}

#[tokio::test]
async fn single_account_keeps_websocket_evidence_before_local_http_failure() {
    for websocket_attempted in [false, true] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/codex/responses"))
            .respond_with(ResponseTemplate::new(401))
            .expect(u64::from(websocket_attempted))
            .mount(&server)
            .await;
        let state = state(&server.uri(), true);
        let route = routing::resolve_model(&state.config, "gpt-6-astra");
        let credential = credential(if websocket_attempted {
            "valid-account"
        } else {
            "invalid\naccount"
        });
        let error = forward_single(
            &state,
            &route,
            None,
            options(credential),
            Some("invalid\nsession"),
        )
        .await
        .unwrap_err();
        assert_eq!(error.response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(error.message, "responses adapter failed");
        assert_eq!(
            error.failure,
            Some(if websocket_attempted {
                AdapterFailure::BeforeHeaders
            } else {
                AdapterFailure::NoUpstreamAttempt
            })
        );
        server.verify().await;
        assert!(server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| request.method == "GET"));
    }
}

#[tokio::test]
async fn local_websocket_header_failure_still_uses_http_on_the_same_account() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let state = state(&server.uri(), true);
    let route = routing::resolve_model(&state.config, "gpt-6-astra");
    let (status, response) =
        forward_single(&state, &route, None, options(credential("café")), None)
            .await
            .unwrap();
    assert_eq!(status, StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["type"], "message");
    server.verify().await;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].headers["chatgpt-account-id"].as_bytes(),
        "café".as_bytes()
    );
}
