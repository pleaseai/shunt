//! Session bindings committed through the Codex Responses pool loops (HTTP
//! loop, committed stream, WebSocket), with the switch metric read back
//! through its test hook (ADR-0008 §2, §8; FR-10, FR-13).
//!
//! Each test runs on its own provider name, because the metric hooks are
//! process-wide and keyed by provider.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::json;
use wiremock::matchers::{header, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::context::{PoolForward, TurnOptions};
use super::forward_chatgpt_oauth;
use crate::{
    accounts::{account_key, AccountPool, QuotaState, StoreFamily},
    config::{AccountConfig, Config, PoolConfig},
    metrics::{session_account_switch_count_for_tests, session_account_switch_total_for_tests},
    routing::{AdapterKind, Route},
    server::AppState,
};

const COMPLETED_TURN: &str = "event: response.created\ndata: {\"response\":{\"id\":\"resp_1\"}}\n\n\
     event: response.output_text.delta\ndata: {\"delta\":\"hi\"}\n\n\
     event: response.completed\ndata: {\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";

/// A ChatGPT access token whose `chatgpt_account_id` claim is `account_id`.
fn token(account_id: &str) -> String {
    let payload = URL_SAFE_NO_PAD.encode(
        json!({"https://api.openai.com/auth": {"chatgpt_account_id": account_id}}).to_string(),
    );
    format!("e30.{payload}.e30")
}

/// Process environment variables set for one test, removed on drop.
struct EnvVars(Vec<String>);

impl Drop for EnvVars {
    fn drop(&mut self) {
        for name in &self.0 {
            std::env::remove_var(name);
        }
    }
}

/// A two-account Codex pool on its own `provider`.
struct Pool {
    state: AppState,
    route: Route,
    accounts: Vec<AccountConfig>,
    _tokens: EnvVars,
}

fn env_name(provider: &str, suffix: &str) -> String {
    format!(
        "SHUNT_TEST_RESPONSES_BINDING_{}_{suffix}",
        provider.to_ascii_uppercase().replace('-', "_")
    )
}

fn start_pool(provider: &str, base_url: String, pool: Option<PoolConfig>) -> Pool {
    let names = [env_name(provider, "A"), env_name(provider, "B")];
    for (name, suffix) in names.iter().zip(["a", "b"]) {
        std::env::set_var(name, token(&format!("{provider}-{suffix}")));
    }
    let tokens = EnvVars(names.to_vec());
    let mut config = Config::default();
    let mut codex = config.providers["codex"].clone();
    codex.base_url = base_url;
    config.providers.insert(provider.to_string(), codex);
    config.server.pool = pool;
    let state = AppState::new(config, reqwest::Client::new()).unwrap();
    let accounts = ["A", "B"]
        .into_iter()
        .map(|suffix| AccountConfig {
            name: format!("{provider}-{}", suffix.to_ascii_lowercase()),
            token_env: Some(env_name(provider, suffix)),
            store_family: Some(StoreFamily::Chatgpt),
            ..Default::default()
        })
        .collect();
    let route = Route {
        provider: provider.to_string(),
        adapter: AdapterKind::Responses,
        model: "gpt-5.2-codex".to_string(),
        upstream_model: "gpt-5.2-codex".to_string(),
        effort: None,
        service_tier: None,
    };
    Pool {
        state,
        route,
        accounts,
        _tokens: tokens,
    }
}

impl Pool {
    fn provider(&self) -> &str {
        &self.route.provider
    }

    /// A session whose first choice, every account healthy, is `index`.
    fn session_for(&self, index: usize) -> String {
        let fresh = AccountPool::new();
        (0..10_000)
            .map(|candidate| format!("{}-session-{candidate}", self.provider()))
            .find(|session| {
                fresh
                    .select_order(self.provider(), &self.accounts, Some(session), None, None)
                    .first()
                    == Some(&index)
            })
            .expect("a session id maps to the requested account")
    }

    /// The account `session` is bound to, read off a fresh selection ticket.
    fn bound(&self, session: &str) -> Option<usize> {
        let (_, ticket) = self.state.accounts.select_order_with_ticket(
            self.provider(),
            &self.accounts,
            Some(session),
            Some(&self.route.upstream_model),
            self.state.config.server.pool.as_ref(),
        );
        let ticket = ticket.expect("a session request carries a ticket");
        self.accounts
            .iter()
            .position(|account| ticket.is_bound_to(account))
    }

    fn bearer(&self, index: usize) -> String {
        let account = &self.accounts[index].name;
        format!("Bearer {}", token(account))
    }

    /// One pool turn for `session`; the body is read to the end, so a
    /// committed stream has run its account loop. Returns the serving account
    /// (from `x-shunt-account`, or the stream's `event: account` frame).
    async fn send(&self, session: &str, stream: bool) -> (StatusCode, String) {
        let forward = PoolForward {
            pool_key: Some(session.to_string()),
            session_id: Some(session.to_string()),
            compact: crate::request::CompactionMark::default(),
            delegation: None,
            upstream_body: std::sync::Arc::new(json!({"input": []})),
            accounts_config: self.accounts.clone(),
            turn: TurnOptions {
                attribution: crate::client_attribution::ClientAttribution::UNATTRIBUTED,
                client_wants_stream: stream,
                thinking_enabled: false,
                tool_search_native: false,
                stop_sequences: Vec::new(),
                response_bounds: crate::adapters::ResponseBounds::default(),
            },
            estimate_input: None,
        };
        let (status, response) =
            match forward_chatgpt_oauth(self.state.clone(), self.route.clone(), forward).await {
                Ok(answer) => answer,
                Err(error) => return (error.response.status(), String::new()),
            };
        let header = response
            .headers()
            .get("x-shunt-account")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let served = header.unwrap_or_else(|| {
            let body = String::from_utf8_lossy(&body);
            self.accounts
                .iter()
                .find(|account| body.contains(&account.name))
                .map(|account| account.name.clone())
                .unwrap_or_default()
        });
        (status, served)
    }
}

async fn answer(upstream: &MockServer, bearer: String, template: ResponseTemplate, times: u64) {
    Mock::given(method("POST"))
        .and(header("authorization", bearer.as_str()))
        .respond_with(template)
        .up_to_n_times(times)
        .mount(upstream)
        .await;
}

fn ok() -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(COMPLETED_TURN)
}

/// HTTP loop: a 429 on the bound account rotates, and the account that serves
/// takes the binding with reason `failover`, not thinking-bound.
#[tokio::test]
async fn http_loop_failover_moves_the_binding() {
    let _env = crate::config::test_env_lock();
    let upstream = MockServer::start().await;
    let pool = start_pool("bind-codex-failover", upstream.uri(), None);
    answer(&upstream, pool.bearer(0), ok(), 1).await;
    answer(&upstream, pool.bearer(0), ResponseTemplate::new(429), 1).await;
    answer(&upstream, pool.bearer(1), ok(), 1).await;
    let session = pool.session_for(0);

    assert_eq!(pool.send(&session, false).await.1, pool.accounts[0].name);
    assert_eq!(pool.bound(&session), Some(0));
    let (status, served) = pool.send(&session, false).await;
    assert_eq!(
        (status, served),
        (StatusCode::OK, pool.accounts[1].name.clone())
    );
    assert_eq!(pool.bound(&session), Some(1));
    assert_eq!(
        session_account_switch_count_for_tests(pool.provider(), "failover", false),
        1
    );
    assert_eq!(session_account_switch_total_for_tests(pool.provider()), 1);
}

/// Committed stream: a cooldown moves the session with reason `cooldown`.
#[tokio::test]
async fn streaming_cooldown_moves_the_binding_once() {
    let _env = crate::config::test_env_lock();
    let upstream = MockServer::start().await;
    let pool = start_pool("bind-codex-stream", upstream.uri(), None);
    answer(&upstream, pool.bearer(0), ok(), 1).await;
    answer(&upstream, pool.bearer(1), ok(), 2).await;
    let session = pool.session_for(0);

    pool.send(&session, true).await;
    assert_eq!(pool.bound(&session), Some(0));
    pool.state.accounts.cooldown(
        pool.provider(),
        &pool.accounts[0],
        Duration::from_secs(300),
        "test",
    );
    pool.send(&session, true).await;
    assert_eq!(pool.bound(&session), Some(1));
    pool.state
        .accounts
        .mark_healthy(pool.provider(), &pool.accounts[0], true);
    pool.send(&session, true).await;
    assert_eq!(pool.bound(&session), Some(1), "no pull back after recovery");
    assert_eq!(
        session_account_switch_count_for_tests(pool.provider(), "cooldown", false),
        1
    );
    assert_eq!(session_account_switch_total_for_tests(pool.provider()), 1);
}

/// A relayed non-2xx is not a serve point, on either transport.
#[tokio::test]
async fn a_relayed_client_error_commits_nothing() {
    let _env = crate::config::test_env_lock();
    let upstream = MockServer::start().await;
    let pool = start_pool("bind-codex-400", upstream.uri(), None);
    answer(
        &upstream,
        pool.bearer(0),
        ResponseTemplate::new(400).set_body_string(r#"{"detail":"bad request"}"#),
        2,
    )
    .await;
    let session = pool.session_for(0);
    for stream in [false, true] {
        pool.send(&session, stream).await;
        assert_eq!(pool.bound(&session), None, "stream: {stream}");
    }
    assert_eq!(session_account_switch_total_for_tests(pool.provider()), 0);
}

/// ADR-0008 §8: a re-probe serving neither places nor moves a binding. An
/// unbound session whose turn the re-probed stale account serves stays
/// unbound; its next, normal serve binds it to the healthy account.
#[tokio::test]
async fn a_reprobe_serve_leaves_the_session_unbound_until_a_normal_serve() {
    let _env = crate::config::test_env_lock();
    let upstream = MockServer::start().await;
    let pool = start_pool(
        "bind-codex-reprobe",
        upstream.uri(),
        Some(PoolConfig {
            default_threshold: Some(0.5),
            reprobe_seconds: Some(60),
            ..Default::default()
        }),
    );
    let session = pool.session_for(0);
    let observed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 61;
    pool.state.accounts.import_quotas([(
        account_key(pool.provider(), &pool.accounts[0]),
        QuotaState {
            utilization_5h: Some(0.9),
            observed_at_5h: Some(observed_at),
            ..Default::default()
        },
    )]);
    answer(&upstream, pool.bearer(0), ok(), 1).await;
    answer(&upstream, pool.bearer(1), ok(), 1).await;

    assert_eq!(
        pool.send(&session, false).await.1,
        pool.accounts[0].name,
        "the stale account is re-probed first"
    );
    assert_eq!(pool.bound(&session), None, "a probe serve binds nothing");
    assert_eq!(pool.send(&session, false).await.1, pool.accounts[1].name);
    assert_eq!(pool.bound(&session), Some(1), "a normal serve binds");
    assert_eq!(session_account_switch_total_for_tests(pool.provider()), 0);
}

/// The WebSocket transport takes its account from the pool's selection and
/// commits the binding when its turn succeeds.
#[tokio::test]
async fn a_websocket_turn_commits_the_binding() {
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tungstenite::Message;

    use crate::adapters::responses::codex_ws::{clear_pool_for_tests, POOL_TEST_LOCK};

    let _env = crate::config::test_env_lock();
    let _pool_guard = POOL_TEST_LOCK.lock().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut ws = tokio_tungstenite::accept_async_with_config(
                    socket,
                    Some(tungstenite::protocol::WebSocketConfig::default()),
                )
                .await
                .unwrap();
                while let Some(Ok(message)) = ws.next().await {
                    match message {
                        Message::Text(_) => {
                            for event in [
                                r#"{"type":"response.created","response":{"id":"resp_1"}}"#,
                                r#"{"type":"response.output_text.delta","delta":"hi"}"#,
                                r#"{"type":"response.completed","response":{}}"#,
                            ] {
                                ws.send(Message::Text(event.to_string().into()))
                                    .await
                                    .unwrap();
                            }
                        }
                        Message::Ping(data) => ws.send(Message::Pong(data)).await.unwrap(),
                        Message::Close(_) => break,
                        _ => {}
                    }
                }
            });
        }
    });
    let mut pool = start_pool("bind-codex-ws", format!("http://{address}"), None);
    let mut config = (*pool.state.config).clone();
    config.providers.get_mut(pool.provider()).unwrap().websocket = true;
    pool.state = AppState::new(config, reqwest::Client::new()).unwrap();
    let session = pool.session_for(1);

    let (status, served) = pool.send(&session, false).await;
    assert_eq!(
        (status, served),
        (StatusCode::OK, pool.accounts[1].name.clone())
    );
    assert_eq!(pool.bound(&session), Some(1));
    assert_eq!(session_account_switch_total_for_tests(pool.provider()), 0);

    clear_pool_for_tests();
    server.abort();
}
