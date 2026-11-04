//! Session bindings committed through the Claude and Kimi pool loops, with the
//! switch metric read back through its test hook (ADR-0008 §2, FR-13).
//!
//! Each test runs on its own provider name, because the metric hooks are
//! process-wide and keyed by provider.

use std::{sync::Arc, time::Duration};

use axum::http::{HeaderMap, StatusCode, Uri};
use wiremock::{
    matchers::{header, method, path},
    Mock, MockServer, ResponseTemplate,
};

use crate::{
    accounts::{AccountPool, CooldownScope},
    config::{AccountConfig, AuthMode, Config},
    metrics::{session_account_switch_count_for_tests, session_account_switch_total_for_tests},
    routing::{AdapterKind, Route},
    server::AppState,
};

/// Process environment variables set for one test, removed on drop.
struct EnvVars(Vec<String>);

impl EnvVars {
    fn set(names_and_values: &[(String, String)]) -> Self {
        for (name, value) in names_and_values {
            std::env::set_var(name, value);
        }
        Self(
            names_and_values
                .iter()
                .map(|(name, _)| name.clone())
                .collect(),
        )
    }
}

impl Drop for EnvVars {
    fn drop(&mut self) {
        for name in &self.0 {
            std::env::remove_var(name);
        }
    }
}

/// A two-account pool on `provider`, its accounts as the request path
/// resolves them, and each account's bearer token.
struct Pool {
    state: AppState,
    provider: &'static str,
    accounts: Vec<AccountConfig>,
    tokens: [String; 2],
    _env: EnvVars,
}

fn env_name(provider: &str, suffix: &str) -> String {
    format!(
        "SHUNT_TEST_BINDING_{}_{suffix}",
        provider.to_ascii_uppercase().replace('-', "_")
    )
}

fn pool_config(provider: &'static str, auth: AuthMode, base_url: &str) -> Config {
    let mut config = Config::default();
    let mut pooled = config
        .providers
        .get("anthropic")
        .expect("built-in anthropic provider")
        .clone();
    pooled.base_url = base_url.to_string();
    pooled.auth = auth;
    pooled.accounts = ["a", "b"]
        .into_iter()
        .map(|name| AccountConfig {
            name: format!("account-{name}"),
            token_env: Some(env_name(provider, &name.to_ascii_uppercase())),
            // Kimi has no verified identity; a uuid only keys Claude accounts.
            uuid: (auth == AuthMode::ClaudeOauth).then(|| format!("uuid-{provider}-{name}")),
            ..Default::default()
        })
        .collect();
    config.providers.insert(provider.to_string(), pooled);
    config
}

async fn start_pool(provider: &'static str, auth: AuthMode, upstream: &MockServer) -> Pool {
    let tokens = [
        format!("fake-{provider}-token-a"),
        format!("fake-{provider}-token-b"),
    ];
    let env = EnvVars::set(&[
        (env_name(provider, "A"), tokens[0].clone()),
        (env_name(provider, "B"), tokens[1].clone()),
    ]);
    let config = pool_config(provider, auth, &upstream.uri());
    let accounts = config.providers[provider]
        .resolve_pool_accounts()
        .await
        .expect("pool accounts resolve");
    let state = AppState::new(config, reqwest::Client::new()).unwrap();
    Pool {
        state,
        provider,
        accounts,
        tokens,
        _env: env,
    }
}

impl Pool {
    /// A session whose first choice, every account healthy, is `index`.
    fn session_for(&self, index: usize) -> String {
        let fresh = AccountPool::new();
        (0..10_000)
            .map(|candidate| format!("{}-session-{candidate}", self.provider))
            .find(|session| {
                fresh
                    .select_order(self.provider, &self.accounts, Some(session), None, None)
                    .first()
                    == Some(&index)
            })
            .expect("a session id maps to the requested account")
    }

    /// The account `session` is bound to, read off a fresh selection ticket.
    fn bound(&self, session: &str, model: &str) -> Option<usize> {
        let (_, ticket) = self.state.accounts.select_order_with_ticket(
            self.provider,
            &self.accounts,
            Some(session),
            Some(model),
            None,
        );
        let ticket = ticket.expect("a session request carries a ticket");
        self.accounts
            .iter()
            .position(|account| ticket.is_bound_to(account))
    }

    fn bearer(&self, index: usize) -> String {
        format!("Bearer {}", self.tokens[index])
    }

    /// Send one request for `session` on `model`; returns the status and the
    /// account that served it.
    async fn send(&self, state: &AppState, session: &str, model: &str) -> (StatusCode, String) {
        let route = Route {
            provider: self.provider.to_string(),
            adapter: AdapterKind::Anthropic,
            model: model.to_string(),
            upstream_model: model.to_string(),
            effort: None,
            service_tier: None,
        };
        let uri: Uri = "/v1/messages".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("x-claude-code-session-id", session.parse().unwrap());
        let body = crate::request::RequestBody::parse(
            serde_json::json!({
                "model": model,
                "max_tokens": 16,
                "messages": [{"role": "user", "content": "hi"}],
            })
            .to_string()
            .into_bytes(),
        )
        .unwrap();
        let (status, response) = match super::forward(
            state.clone(),
            route,
            &uri,
            &headers,
            body,
            crate::adapters::ResponseBounds::default(),
        )
        .await
        {
            Ok(answer) => answer,
            Err(error) => panic!("the pool should answer: {}", error.message),
        };
        let served = response
            .headers()
            .get("x-shunt-account")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        (status, served)
    }
}

async fn answer(upstream: &MockServer, bearer: String, template: ResponseTemplate, times: u64) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", bearer.as_str()))
        .respond_with(template)
        .up_to_n_times(times)
        .mount(upstream)
        .await;
}

fn ok() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#)
}

fn quota_rejected() -> ResponseTemplate {
    ResponseTemplate::new(429)
        .insert_header("retry-after", "300")
        .insert_header("anthropic-ratelimit-unified-5h-status", "rejected")
        .set_body_string(r#"{"error":"quota exhausted"}"#)
}

/// AC-002: a cooldown moves the session with reason `cooldown`, and it stays
/// on its new account after the old one recovers. Sonnet 5.5 is
/// thinking-bound.
#[tokio::test]
async fn a_cooldown_moves_the_session_once_and_it_stays_after_recovery() {
    let _lock = crate::config::test_env_lock();
    let upstream = MockServer::start().await;
    let pool = start_pool("bind-claude-cooldown", AuthMode::ClaudeOauth, &upstream).await;
    answer(&upstream, pool.bearer(0), ok(), 1).await;
    answer(&upstream, pool.bearer(1), ok(), 2).await;
    let model = "claude-sonnet-5-5";
    let session = pool.session_for(0);

    assert_eq!(pool.send(&pool.state, &session, model).await.1, "account-a");
    assert_eq!(pool.bound(&session, model), Some(0));

    pool.state.accounts.cooldown(
        pool.provider,
        &pool.accounts[0],
        Duration::from_secs(300),
        "test",
    );
    assert_eq!(pool.send(&pool.state, &session, model).await.1, "account-b");
    assert_eq!(pool.bound(&session, model), Some(1));

    pool.state
        .accounts
        .mark_healthy(pool.provider, &pool.accounts[0], true);
    assert_eq!(pool.send(&pool.state, &session, model).await.1, "account-b");
    assert_eq!(pool.bound(&session, model), Some(1));

    assert_eq!(
        session_account_switch_count_for_tests(pool.provider, "cooldown", true),
        1
    );
    assert_eq!(session_account_switch_total_for_tests(pool.provider), 1);
}

/// The bound account answers a quota-rejected 429 and the next account
/// serves: the binding moves with reason `failover`. Opus is not
/// thinking-bound.
#[tokio::test]
async fn a_failed_attempt_on_the_bound_account_moves_it_as_failover() {
    let _lock = crate::config::test_env_lock();
    let upstream = MockServer::start().await;
    let pool = start_pool("bind-claude-failover", AuthMode::ClaudeOauth, &upstream).await;
    answer(&upstream, pool.bearer(0), ok(), 1).await;
    answer(&upstream, pool.bearer(0), quota_rejected(), 1).await;
    answer(&upstream, pool.bearer(1), ok(), 1).await;
    let model = "claude-opus-5-5";
    let session = pool.session_for(0);

    assert_eq!(pool.send(&pool.state, &session, model).await.1, "account-a");
    let (status, served) = pool.send(&pool.state, &session, model).await;
    assert_eq!((status, served.as_str()), (StatusCode::OK, "account-b"));
    assert_eq!(pool.bound(&session, model), Some(1));
    assert_eq!(
        session_account_switch_count_for_tests(pool.provider, "failover", false),
        1
    );
    assert_eq!(session_account_switch_total_for_tests(pool.provider), 1);
}

/// AC-003: a 429 without a `rejected` status retries the same account, and
/// the binding stays with no switch recorded.
#[tokio::test]
async fn a_throttle_retry_on_the_bound_account_keeps_the_binding() {
    let _lock = crate::config::test_env_lock();
    let upstream = MockServer::start().await;
    let pool = start_pool("bind-claude-throttle", AuthMode::ClaudeOauth, &upstream).await;
    answer(&upstream, pool.bearer(0), ok(), 1).await;
    answer(
        &upstream,
        pool.bearer(0),
        ResponseTemplate::new(429).insert_header("retry-after", "0"),
        1,
    )
    .await;
    answer(&upstream, pool.bearer(0), ok(), 1).await;
    let model = "claude-sonnet-5-5";
    let session = pool.session_for(0);

    assert_eq!(pool.send(&pool.state, &session, model).await.1, "account-a");
    let (status, served) = pool.send(&pool.state, &session, model).await;
    assert_eq!((status, served.as_str()), (StatusCode::OK, "account-a"));
    assert_eq!(pool.bound(&session, model), Some(0));
    assert_eq!(session_account_switch_total_for_tests(pool.provider), 0);
}

/// AC-004: the bound account cools down and so does every other one, so no
/// healthy alternative exists: the binding stays and no switch is recorded.
#[tokio::test]
async fn no_alternative_keeps_the_binding_without_a_switch() {
    let _lock = crate::config::test_env_lock();
    let upstream = MockServer::start().await;
    let pool = start_pool(
        "bind-claude-no-alternative",
        AuthMode::ClaudeOauth,
        &upstream,
    )
    .await;
    answer(&upstream, pool.bearer(0), ok(), 2).await;
    answer(&upstream, pool.bearer(1), ok(), 1).await;
    let model = "claude-sonnet-5-5";
    let session = pool.session_for(0);

    assert_eq!(pool.send(&pool.state, &session, model).await.1, "account-a");
    for account in &pool.accounts {
        pool.state
            .accounts
            .cooldown(pool.provider, account, Duration::from_secs(300), "test");
    }
    let (_, ticket) = pool.state.accounts.select_order_with_ticket(
        pool.provider,
        &pool.accounts,
        Some(&session),
        Some(model),
        None,
    );
    assert_eq!(
        ticket.unwrap().prior_ineligible(),
        Some(crate::accounts::SwitchReason::Cooldown)
    );
    let (status, _) = pool.send(&pool.state, &session, model).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pool.bound(&session, model), Some(0));
    assert_eq!(session_account_switch_total_for_tests(pool.provider), 0);
}

/// AC-005: a Fable request finds its bound account limited only by the
/// Fable-scoped window: another account serves it and the session rebinds
/// with reason `cooldown`, while a non-Fable request before it stayed put.
#[tokio::test]
async fn a_fable_scoped_limit_moves_the_fable_request() {
    let _lock = crate::config::test_env_lock();
    let upstream = MockServer::start().await;
    let pool = start_pool("bind-claude-fable", AuthMode::ClaudeOauth, &upstream).await;
    answer(&upstream, pool.bearer(0), ok(), 2).await;
    answer(&upstream, pool.bearer(1), ok(), 1).await;
    let session = pool.session_for(0);

    assert_eq!(
        pool.send(&pool.state, &session, "claude-fable-5").await.1,
        "account-a"
    );
    pool.state.accounts.cooldown_scoped(
        pool.provider,
        &pool.accounts[0],
        Duration::from_secs(300),
        "test",
        CooldownScope::Fable,
    );
    assert_eq!(
        pool.send(&pool.state, &session, "claude-sonnet-5-5")
            .await
            .1,
        "account-a",
        "a non-Fable request is not limited by the Fable window"
    );
    assert_eq!(session_account_switch_total_for_tests(pool.provider), 0);
    assert_eq!(
        pool.send(&pool.state, &session, "claude-fable-5").await.1,
        "account-b"
    );
    assert_eq!(pool.bound(&session, "claude-fable-5"), Some(1));
    assert_eq!(
        session_account_switch_count_for_tests(pool.provider, "cooldown", false),
        1
    );
    assert_eq!(session_account_switch_total_for_tests(pool.provider), 1);
}

/// AC-018: a reload drops the bound account; the next request rebinds with
/// reason `failover`.
#[tokio::test]
async fn a_reload_removing_the_bound_account_rebinds_as_failover() {
    let _lock = crate::config::test_env_lock();
    let upstream = MockServer::start().await;
    let pool = start_pool("bind-claude-reload", AuthMode::ClaudeOauth, &upstream).await;
    answer(&upstream, pool.bearer(0), ok(), 1).await;
    answer(&upstream, pool.bearer(1), ok(), 1).await;
    let model = "claude-sonnet-5-5";
    let session = pool.session_for(0);
    assert_eq!(pool.send(&pool.state, &session, model).await.1, "account-a");

    // The per-request snapshot a hot reload swaps in: same process-lifetime
    // pool, a config without account-a.
    let mut config = (*pool.state.config).clone();
    config
        .providers
        .get_mut(pool.provider)
        .unwrap()
        .accounts
        .remove(0);
    let mut reloaded = pool.state.clone();
    reloaded.config = Arc::new(config);
    assert_eq!(pool.send(&reloaded, &session, model).await.1, "account-b");
    assert_eq!(
        session_account_switch_count_for_tests(pool.provider, "failover", true),
        1
    );
    assert_eq!(session_account_switch_total_for_tests(pool.provider), 1);
}

/// A relayed 4xx is not a serve point: the session stays unbound.
#[tokio::test]
async fn a_relayed_client_error_commits_nothing() {
    let _lock = crate::config::test_env_lock();
    let upstream = MockServer::start().await;
    let pool = start_pool("bind-claude-relayed-400", AuthMode::ClaudeOauth, &upstream).await;
    answer(
        &upstream,
        pool.bearer(0),
        ResponseTemplate::new(400).set_body_string(
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages: field required"}}"#,
        ),
        1,
    )
    .await;
    let model = "claude-sonnet-5-5";
    let session = pool.session_for(0);
    let (status, served) = pool.send(&pool.state, &session, model).await;
    assert_eq!(
        (status, served.as_str()),
        (StatusCode::BAD_REQUEST, "account-a")
    );
    assert_eq!(pool.bound(&session, model), None);
    assert_eq!(session_account_switch_total_for_tests(pool.provider), 0);
}

/// Kimi: a 402 on the bound account fails over and moves the binding as
/// `failover`; Kimi is never thinking-bound, whatever the model is called.
#[tokio::test]
async fn kimi_failover_moves_the_binding_and_is_not_thinking_bound() {
    let _lock = crate::config::test_env_lock();
    let upstream = MockServer::start().await;
    let pool = start_pool("bind-kimi-failover", AuthMode::KimiOauth, &upstream).await;
    answer(&upstream, pool.bearer(0), ok(), 1).await;
    answer(
        &upstream,
        pool.bearer(0),
        ResponseTemplate::new(402).set_body_string(r#"{"error":"membership inactive"}"#),
        1,
    )
    .await;
    answer(&upstream, pool.bearer(1), ok(), 1).await;
    let model = "claude-sonnet-5-5";
    let session = pool.session_for(0);

    assert_eq!(pool.send(&pool.state, &session, model).await.1, "account-a");
    assert_eq!(pool.send(&pool.state, &session, model).await.1, "account-b");
    assert_eq!(pool.bound(&session, model), Some(1));
    assert_eq!(
        session_account_switch_count_for_tests(pool.provider, "failover", false),
        1
    );
    assert_eq!(session_account_switch_total_for_tests(pool.provider), 1);
}

/// Kimi, AC-002: a cooldown moves the session as `cooldown`, and it stays
/// after the old account recovers.
#[tokio::test]
async fn kimi_cooldown_moves_the_session_once() {
    let _lock = crate::config::test_env_lock();
    let upstream = MockServer::start().await;
    let pool = start_pool("bind-kimi-cooldown", AuthMode::KimiOauth, &upstream).await;
    answer(&upstream, pool.bearer(0), ok(), 1).await;
    answer(&upstream, pool.bearer(1), ok(), 2).await;
    let model = "kimi-for-coding";
    let session = pool.session_for(0);

    assert_eq!(pool.send(&pool.state, &session, model).await.1, "account-a");
    pool.state.accounts.cooldown(
        pool.provider,
        &pool.accounts[0],
        Duration::from_secs(300),
        "test",
    );
    assert_eq!(pool.send(&pool.state, &session, model).await.1, "account-b");
    pool.state
        .accounts
        .mark_healthy(pool.provider, &pool.accounts[0], true);
    assert_eq!(pool.send(&pool.state, &session, model).await.1, "account-b");
    assert_eq!(
        session_account_switch_count_for_tests(pool.provider, "cooldown", false),
        1
    );
    assert_eq!(session_account_switch_total_for_tests(pool.provider), 1);
}
