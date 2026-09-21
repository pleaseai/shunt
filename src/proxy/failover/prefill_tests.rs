//! Admission before the prefill drive (issue #633), feature-on only.
//!
//! The lane's own tests (`routing::prefill::tests`) stop at the algorithm
//! boundary. These drive the whole `forward` path with a counting stub in the
//! algorithm's place, because what #633 is about is *when* that boundary is
//! crossed: upstream's algorithm runs encoder inference over the caller's
//! transcript and writes a session affinity keyed on the caller's
//! `x-claude-code-session-id` as it decides, so the count of calls the stub
//! sees is the count of both.
//!
//! Non-vacuity: move `routing::prefill::decide` back ahead of
//! `check_inbound_auth` in `forward` and both refusal tests go red on the
//! stub's call count; drop `drive_prefill` from the admission condition and
//! `an_unauthenticated_turn_is_refused_before_the_drive` goes red on a `200`
//! instead, because the provisional default-target chain alone is what would
//! be gated. The admitted twin is what keeps either from being satisfied by a
//! gate that refuses everything.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use serde_json::json;
use switchyard_libsy::{Algorithm, Driver, RoutingOutcome};
use switchyard_protocol::{ModelId, Request};

use crate::auth::inbound::InboundAuth;
use crate::config::{AuthMode, Config, ModelConfig, PrefillRouterConfig, RouterConfig};
use crate::gateway::{approval::Identity, jwt, managed, GatewayAuth};
use crate::routing::prefill::PrefillRouters;
use crate::server::AppState;

const ROUTER_ID: &str = "claude-prefill";
const FAST: &str = "fast-alias";
const STRONG: &str = "strong-alias";
/// The issue's shape: the default target (`fast-alias`, the first head) is
/// passthrough, and only the target the algorithm actually picks injects a
/// credential. Gated against the provisional default-target chain alone, an
/// unauthenticated caller would be admitted and driven; gated against the
/// envelope, `[server.auth]` is demanded because *some* target injects.
const PASSTHROUGH_PROVIDER: &str = "prefill-admission-open";
const PROVIDER: &str = "prefill-admission-upstream";
const PROVIDER_KEY_ENV: &str = "prefill-admission-upstream_key";
const CLIENT_TOKEN: &str = "prefill-admission-client-token";
/// A session id the caller asserts and never proved: the value the affinity
/// write would be keyed on.
const SESSION: &str = "session-the-caller-does-not-own";
const GATEWAY_URL: &str = "https://gateway.example";
const GATEWAY_SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

/// Counts every drive and records the session id each one was keyed on —
/// the two facts an affinity write is made of.
struct CountingAlgorithm {
    calls: AtomicUsize,
    sessions: Mutex<Vec<Option<String>>>,
}

#[async_trait::async_trait]
impl Algorithm for CountingAlgorithm {
    fn name(&self) -> &str {
        "prefill_router"
    }

    async fn route(
        self: Arc<Self>,
        _driver: Driver,
        request: Request,
    ) -> switchyard_libsy::Result<RoutingOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.sessions.lock().expect("stub lock").push(
            request
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.session_id.clone()),
        );
        Ok(RoutingOutcome::route_to(
            ModelId::from(STRONG),
            vec![],
            request,
        ))
    }
}

fn mapped(id: &str, provider: &str) -> ModelConfig {
    ModelConfig {
        subagents: None,
        id: id.to_string(),
        display_name: None,
        upstream_model: Some(
            [(provider.to_string(), "gpt-5.2-codex".to_string())]
                .into_iter()
                .collect(),
        ),
        router: None,
        stage_router: None,
    }
}

/// `claude-prefill` over a passthrough `fast-alias` and a credential-injecting
/// `strong-alias`, both Responses upstreams at `base_url`.
fn config(base_url: String) -> Config {
    let mut config = Config::default();
    let mut open = config
        .providers
        .get("codex")
        .expect("codex provider is built in")
        .clone();
    open.base_url = base_url.clone();
    open.auth = AuthMode::Passthrough;
    config
        .providers
        .insert(PASSTHROUGH_PROVIDER.to_string(), open);
    config.providers.insert(
        PROVIDER.to_string(),
        config
            .providers
            .get("codex")
            .expect("codex provider is built in")
            .clone(),
    );
    let provider = config.providers.get_mut(PROVIDER).expect("just inserted");
    provider.base_url = base_url;
    provider.auth = AuthMode::ApiKey;
    provider.api_key_env = Some(PROVIDER_KEY_ENV.to_string());
    config.models = vec![
        ModelConfig {
            subagents: None,
            id: ROUTER_ID.to_string(),
            display_name: None,
            upstream_model: None,
            router: Some(RouterConfig::PrefillRouter(PrefillRouterConfig {
                targets: vec![FAST.to_string(), STRONG.to_string()],
                checkpoint: std::path::PathBuf::from("router.pt"),
                device: None,
                cache_dir: None,
                max_length: None,
                batch_size: None,
            })),
            stage_router: None,
        },
        mapped(FAST, PASSTHROUGH_PROVIDER),
        mapped(STRONG, PROVIDER),
    ];
    config
}

/// A state whose prefill entry is served by the counting stub.
///
/// `AppState::new` builds the real algorithms from the config, which would
/// load `router.pt` through Python — so the state is booted from the config
/// *without* the entry and the snapshot fields are then pointed at the full
/// config and the stub, the same two fields `forward` reads.
fn state(base_url: String) -> (AppState, Arc<CountingAlgorithm>) {
    let full = config(base_url);
    let mut boot = full.clone();
    boot.models.retain(|model| model.id != ROUTER_ID);
    let mut state = AppState::new(boot, reqwest::Client::new()).unwrap();
    let stub = Arc::new(CountingAlgorithm {
        calls: AtomicUsize::new(0),
        sessions: Mutex::new(Vec::new()),
    });
    let algorithm: Arc<dyn Algorithm> = Arc::clone(&stub) as Arc<dyn Algorithm>;
    state.config = Arc::new(full);
    state.prefill_routers = Arc::new(PrefillRouters::from_algorithm(
        ROUTER_ID,
        &[FAST, STRONG],
        algorithm,
    ));
    state.inbound_auth = Some(Arc::new(InboundAuth::new(
        HeaderName::from_static("x-shunt-token"),
        vec![("client".to_string(), CLIENT_TOKEN.to_string())],
    )));
    (state, stub)
}

fn headers(client_token: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert(
        "x-claude-code-session-id",
        HeaderValue::from_static(SESSION),
    );
    if let Some(token) = client_token {
        headers.insert("x-shunt-token", token.parse().unwrap());
    }
    headers
}

async fn forward(
    state: AppState,
    headers: &HeaderMap,
) -> Result<(StatusCode, axum::response::Response), super::ForwardError> {
    let uri: axum::http::Uri = "/v1/messages".parse().unwrap();
    let body = axum::body::Body::from(
        json!({
            "model": ROUTER_ID,
            "stream": true,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "route me"}],
        })
        .to_string(),
    );
    super::forward(state, &uri, headers, body, std::time::Instant::now()).await
}

/// The defect: with `[server.auth]` configured, a caller with no credential
/// naming the prefill id used to run inference and seed an affinity for the
/// session id it chose. Now it is refused first, and the algorithm never
/// sees the request.
#[tokio::test]
async fn an_unauthenticated_turn_is_refused_before_the_drive() {
    let (state, stub) = state("http://127.0.0.1:9".to_string());

    let refused = match forward(state, &headers(None)).await {
        Ok((status, _)) => panic!("an unauthenticated prefill turn was served: {status}"),
        Err(error) => error,
    };

    assert_eq!(refused.response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        stub.calls.load(Ordering::SeqCst),
        0,
        "no inference and no affinity write for a caller the gate refused"
    );
    assert!(stub.sessions.lock().unwrap().is_empty());
}

/// The second gate, on the same side of the drive as the first: a gateway
/// login whose managed policy does not list the prefill id is refused with
/// the policy's error, and the algorithm never sees the request either.
#[tokio::test]
async fn a_policy_denied_turn_is_refused_before_the_drive() {
    let (mut state, stub) = state("http://127.0.0.1:9".to_string());
    state.gateway_auth = Some(Arc::new(
        GatewayAuth::with_optional_approval(
            GATEWAY_URL.to_string(),
            GATEWAY_SECRET.to_vec(),
            3600,
            false,
            None,
        )
        .with_managed_policies(
            Some(vec![managed::ResolvedPolicy {
                emails: None,
                // The prefill id is deliberately absent from the allow list.
                settings: json!({"availableModels": ["some-other-model"]}),
            }]),
            managed::TelemetryPush::default(),
        ),
    ));
    let bearer = jwt::mint(
        &Identity {
            sub: "dev".to_string(),
            email: "dev@example.com".to_string(),
            name: "Dev".to_string(),
        },
        GATEWAY_URL,
        GATEWAY_SECRET,
        3600,
    );
    let mut request_headers = headers(None);
    request_headers.insert("authorization", format!("Bearer {bearer}").parse().unwrap());

    let refused = match forward(state, &request_headers).await {
        Ok((status, _)) => panic!("a policy-denied prefill turn was served: {status}"),
        Err(error) => error,
    };

    assert_eq!(refused.response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        stub.calls.load(Ordering::SeqCst),
        0,
        "the policy refused the turn, so the algorithm must not have been driven"
    );
}

/// The admitted twin: the same request with the configured token is driven
/// exactly once, keyed on the session it sent, dispatched to the target the
/// algorithm chose rather than the provisional default, and stamped as a
/// prefill decision.
#[tokio::test]
async fn an_admitted_turn_is_driven_once_and_routed_to_the_decision() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let sse = concat!(
        "event: response.created\n",
        "data: {\"response\":{\"id\":\"resp_1\"}}\n\n",
        "event: response.completed\n",
        "data: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
    );
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse.to_string()))
        .expect(1)
        .mount(&server)
        .await;
    let _env = crate::auth::shared::EnvVarGuard::set(PROVIDER_KEY_ENV, "upstream-key");
    let (state, stub) = state(server.uri());

    let (status, response) = match forward(state, &headers(Some(CLIENT_TOKEN))).await {
        Ok(result) => result,
        Err(error) => panic!("an admitted prefill turn was refused: {}", error.message),
    };

    assert_eq!(status, StatusCode::OK);
    // The streaming chain commits its `200` before the upstream send: the
    // body is what drives the round-trip, so it is drained before the mock's
    // expectation is checked.
    let stamped = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body is readable");
    assert!(
        String::from_utf8_lossy(&bytes).contains("event: message_stop"),
        "the admitted turn streams to completion"
    );
    assert_eq!(
        stamped["x-gateway-routed-model"], STRONG,
        "the drive's answer, not the provisional default target, is dispatched"
    );
    assert_eq!(stamped["x-gateway-route-source"], "prefill");
    assert_eq!(stub.calls.load(Ordering::SeqCst), 1, "driven exactly once");
    assert_eq!(
        *stub.sessions.lock().unwrap(),
        vec![Some(SESSION.to_string())],
        "the affinity is keyed on the session the admitted caller sent"
    );
    server.verify().await;
}
