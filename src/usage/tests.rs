use axum::{extract::State, http::HeaderMap};
use serde_json::{json, Value};

use crate::{
    accounts::{AccountSnapshot, UsageSnapshot, UsageWindow},
    config::{AccountConfig, InboundAuthConfig, UsageEndpointConfig},
    server::AppState,
};

use super::{aggregate, get};

/// A seen account snapshot with the given per-window utilization; all other
/// fields default to an available, non-disabled account.
fn snapshot(
    name: &str,
    util_5h: Option<f64>,
    reset_5h: Option<u64>,
    util_7d: Option<f64>,
) -> AccountSnapshot {
    AccountSnapshot {
        name: name.to_string(),
        has_state: true,
        available: true,
        near_quota: false,
        cooldown_secs_remaining: None,
        cooldown_fable_secs_remaining: None,
        priority: 100,
        disabled: false,
        headroom_secs: None,
        utilization_5h: util_5h,
        reset_5h,
        utilization_7d: util_7d,
        reset_7d: None,
        utilization_7d_oi: None,
        reset_7d_oi: None,
        status: None,
    }
}

#[test]
fn aggregate_reports_least_utilized_headroom_per_window() {
    // Two accounts; the least-utilized (0.25) drives 5h headroom and reset.
    let snapshots = vec![
        snapshot("acct-a", Some(0.60), Some(111), Some(0.40)),
        snapshot("acct-b", Some(0.25), Some(222), Some(0.90)),
    ];
    let body = serde_json::to_value(aggregate(&snapshots)).unwrap();
    assert_eq!(body["pool"]["status"], "ok");
    assert_eq!(body["pool"]["windows"]["5h"]["remaining"], json!(0.75));
    assert_eq!(body["pool"]["windows"]["5h"]["resets_at"], json!(222));
    // 7d: least-utilized is 0.40 → remaining 0.60.
    assert_eq!(body["pool"]["windows"]["7d"]["remaining"], json!(0.60));
    // No account reports the Fable window → null.
    assert_eq!(body["pool"]["windows"]["fable"]["remaining"], Value::Null);
    assert_eq!(body["pool"]["windows"]["fable"]["resets_at"], Value::Null);
}

#[test]
fn aggregate_ignores_non_finite_window_utilization() {
    let snapshots = [snapshot("acct-a", Some(f64::NAN), Some(111), None)];
    let body = serde_json::to_value(aggregate(&snapshots)).unwrap();
    assert_eq!(body["pool"]["windows"]["5h"]["remaining"], Value::Null);
    assert_eq!(body["pool"]["windows"]["5h"]["resets_at"], Value::Null);
}

#[test]
fn aggregate_excludes_disabled_accounts_and_null_windows() {
    // The only account with 5h data is disabled → the window reads null
    // (disabled accounts never serve, so their headroom is irrelevant).
    let mut disabled = snapshot("backup", Some(0.10), Some(1), None);
    disabled.disabled = true;
    let unreported = snapshot("live", None, None, None);
    let body = serde_json::to_value(aggregate(&[disabled, unreported])).unwrap();
    assert_eq!(body["pool"]["windows"]["5h"]["remaining"], Value::Null);
}

#[test]
fn aggregate_status_is_exhausted_when_no_selectable_account_exists() {
    let mut disabled = snapshot("acct-a", Some(0.10), None, None);
    disabled.disabled = true;
    let body = serde_json::to_value(aggregate(&[disabled])).unwrap();
    assert_eq!(body["pool"]["status"], "exhausted");
}

#[test]
fn aggregate_status_is_exhausted_when_no_account_available() {
    let mut a = snapshot("acct-a", Some(0.99), None, None);
    a.available = false;
    a.near_quota = true;
    let body = serde_json::to_value(aggregate(&[a])).unwrap();
    assert_eq!(body["pool"]["status"], "exhausted");
}

#[test]
fn aggregate_status_is_degraded_when_near_quota_but_available() {
    let mut a = snapshot("acct-a", Some(0.90), None, None);
    a.near_quota = true; // still available (a backup remains), but flagged
    let b = snapshot("acct-b", Some(0.10), None, None);
    let body = serde_json::to_value(aggregate(&[a, b])).unwrap();
    assert_eq!(body["pool"]["status"], "degraded");
}

#[test]
fn aggregate_never_exposes_account_identity_or_capacity() {
    // Sanitization guarantee: no account name, count, priority, disabled
    // flag, threshold, or headroom appears in the serialized response.
    let mut disabled = snapshot("secret-backup", Some(0.10), Some(1), Some(0.2));
    disabled.disabled = true;
    disabled.priority = 5;
    disabled.headroom_secs = Some(4242);
    let snapshots = vec![
        snapshot("secret-primary", Some(0.30), Some(9), Some(0.50)),
        disabled,
    ];
    let text = serde_json::to_string(&aggregate(&snapshots)).unwrap();
    for leak in [
        "secret-primary",
        "secret-backup",
        "name",
        "priority",
        "disabled",
        "threshold",
        "headroom",
        "cooldown",
    ] {
        assert!(
            !text.contains(leak),
            "usage response leaked {leak:?}: {text}"
        );
    }
}

/// Config with `[server.auth]` bound to a unique env var and `[server.usage]`
/// enabled, plus the built-in `codex` provider given one explicit account so
/// the snapshot path does not touch the account store. Seeds authoritative
/// usage with **future** resets (a past reset would be cleared as stale by
/// the snapshot). Returns the state, the env var name (caller removes it),
/// and the seeded 5h reset for assertion.
fn state_with_auth_and_seeded_pool(token: &str, label: &str) -> (AppState, String, u64) {
    // Per-test-unique name: tests share the process env, and one test's
    // `remove_var` must not race another's construction-time resolve.
    let env = format!("SHUNT_USAGE_TEST_TOKENS_{}_{label}", std::process::id());
    std::env::set_var(&env, format!("tester:{token}"));
    let reset_5h = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3_600;
    let mut config = crate::config::Config::default();
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: env.clone(),
    });
    config.server.usage = Some(UsageEndpointConfig::default());
    let account = AccountConfig {
        name: "acct-a".to_string(),
        ..AccountConfig::default()
    };
    config
        .providers
        .get_mut("codex")
        .expect("built-in codex provider")
        .accounts = vec![account.clone()];
    let state = AppState::new(config, reqwest::Client::new()).unwrap();
    // Seed the same response-derived header groups that the Codex adapters
    // pass to `note_codex_quota` in production. The 300-minute and 10080-minute
    // groups map to the 5-hour and shared weekly windows respectively.
    let mut headers = HeaderMap::new();
    headers.insert("x-codex-primary-used-percent", "25".parse().unwrap());
    headers.insert("x-codex-primary-window-minutes", "300".parse().unwrap());
    headers.insert(
        "x-codex-primary-reset-at",
        reset_5h.to_string().parse().unwrap(),
    );
    headers.insert("x-codex-secondary-used-percent", "40".parse().unwrap());
    headers.insert("x-codex-secondary-window-minutes", "10080".parse().unwrap());
    headers.insert(
        "x-codex-secondary-reset-at",
        (reset_5h + 3_600).to_string().parse().unwrap(),
    );
    state.accounts.note_codex_quota("codex", &account, &headers);
    (state, env, reset_5h)
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn serves_aggregate_to_an_authenticated_client() {
    let (state, env, reset_5h) = state_with_auth_and_seeded_pool("tok-secret", "serves");
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", "tok-secret".parse().unwrap());

    let response = get(State(state), headers).await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = body_json(response).await;
    std::env::remove_var(&env);

    assert_eq!(body["pool"]["status"], "ok");
    assert_eq!(body["pool"]["windows"]["5h"]["remaining"], json!(0.75));
    assert_eq!(body["pool"]["windows"]["5h"]["resets_at"], json!(reset_5h));
    assert_eq!(body["pool"]["windows"]["7d"]["remaining"], json!(0.60));
    assert_eq!(
        body["pool"]["windows"]["7d"]["resets_at"],
        json!(reset_5h + 3_600)
    );
    assert_eq!(body["pool"]["windows"]["fable"]["remaining"], Value::Null);
}

#[tokio::test]
async fn aggregates_codex_headers_and_claude_fable_usage_together() {
    use crate::accounts::StoreFamily;
    use crate::config::{ApiKeyHeader, AuthMode, CountTokens, ProviderConfig, ProviderKind};

    let env = format!(
        "SHUNT_USAGE_TEST_TOKENS_{}_codex_claude",
        std::process::id()
    );
    std::env::set_var(&env, "tester:tok-secret");
    let reset_5h = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3_600;
    let reset_7d = reset_5h + 3_600;
    let reset_fable = reset_7d + 3_600;

    let mut config = crate::config::Config::default();
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: env.clone(),
    });
    config.server.usage = Some(UsageEndpointConfig::default());

    let codex_account = AccountConfig {
        name: "codex-a".to_string(),
        ..AccountConfig::default()
    };
    config
        .providers
        .get_mut("codex")
        .expect("built-in codex provider")
        .accounts = vec![codex_account.clone()];

    let claude_account = AccountConfig {
        name: "claude-a".to_string(),
        ..AccountConfig::default()
    };
    config.providers.insert(
        "claude-oauth".to_string(),
        ProviderConfig {
            kind: ProviderKind::Anthropic,
            base_url: "https://api.anthropic.com".to_string(),
            auth: AuthMode::ClaudeOauth,
            api_key_env: None,
            api_key_header: ApiKeyHeader::Bearer,
            effort: None,
            service_tier: None,
            count_tokens: CountTokens::default(),
            accounts: vec![claude_account.clone()],
            account_scope: Vec::new(),
            websocket: false,
            tool_search: None,
            request_compression: true,
            retry: Default::default(),
            workspace_roots: Vec::new(),
            sandbox: true,
        },
    );

    let state = AppState::new(config, reqwest::Client::new()).unwrap();
    let codex_account = AccountConfig {
        store_family: Some(StoreFamily::Chatgpt),
        ..codex_account
    };
    let claude_account = AccountConfig {
        store_family: Some(StoreFamily::Claude),
        ..claude_account
    };

    // Codex usage is response-derived. Claude's Fable value follows the same
    // authoritative `note_usage` path used by its OAuth usage poller.
    let mut codex_headers = HeaderMap::new();
    codex_headers.insert("x-codex-primary-used-percent", "25".parse().unwrap());
    codex_headers.insert("x-codex-primary-window-minutes", "300".parse().unwrap());
    codex_headers.insert(
        "x-codex-primary-reset-at",
        reset_5h.to_string().parse().unwrap(),
    );
    codex_headers.insert("x-codex-secondary-used-percent", "40".parse().unwrap());
    codex_headers.insert("x-codex-secondary-window-minutes", "10080".parse().unwrap());
    codex_headers.insert(
        "x-codex-secondary-reset-at",
        reset_7d.to_string().parse().unwrap(),
    );
    state
        .accounts
        .note_codex_quota("codex", &codex_account, &codex_headers);
    state.accounts.note_usage(
        "claude-oauth",
        &claude_account,
        &UsageSnapshot {
            five_hour: None,
            seven_day: None,
            seven_day_oi: Some(UsageWindow {
                utilization: 0.15,
                resets_at: Some(reset_fable),
            }),
        },
    );

    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", "tok-secret".parse().unwrap());
    let response = get(State(state), headers).await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = body_json(response).await;
    std::env::remove_var(&env);

    assert_eq!(body["pool"]["windows"]["5h"]["remaining"], json!(0.75));
    assert_eq!(body["pool"]["windows"]["5h"]["resets_at"], json!(reset_5h));
    assert_eq!(body["pool"]["windows"]["7d"]["remaining"], json!(0.60));
    assert_eq!(body["pool"]["windows"]["7d"]["resets_at"], json!(reset_7d));
    assert_eq!(body["pool"]["windows"]["fable"]["remaining"], json!(0.85));
    assert_eq!(
        body["pool"]["windows"]["fable"]["resets_at"],
        json!(reset_fable)
    );
}

/// `GET /usage` must cover a `kimi_oauth` pool, not just Claude and Codex.
/// The handler filters providers by auth mode before resolving accounts, and
/// that filter is an explicit enumeration — the same shape that had already
/// dropped Kimi from `providers.accounts` validation and from `/admin/pool`.
///
/// Kimi is seeded *less* utilized than the codex account, so Kimi is the one
/// that drives the reported headroom: were Kimi filtered out, 5h remaining
/// would fall back to codex's 0.75. Both accounts are seeded with the
/// `store_family` the pool path stamps on them in `resolve_pool_accounts`,
/// because `account_key` keys pool state by family — seeding an unstamped
/// account would file the usage under a different key than the handler reads.
#[tokio::test]
async fn aggregate_covers_a_kimi_oauth_pool_alongside_claude_and_codex() {
    use crate::accounts::StoreFamily;
    use crate::config::{ApiKeyHeader, AuthMode, CountTokens, ProviderConfig, ProviderKind};

    let env = format!("SHUNT_USAGE_TEST_TOKENS_{}_kimi", std::process::id());
    std::env::set_var(&env, "tester:tok-secret");
    let reset_5h = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3_600;

    let mut config = crate::config::Config::default();
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: env.clone(),
    });
    config.server.usage = Some(UsageEndpointConfig::default());

    let codex_account = AccountConfig {
        name: "codex-a".to_string(),
        ..AccountConfig::default()
    };
    config
        .providers
        .get_mut("codex")
        .expect("built-in codex provider")
        .accounts = vec![codex_account.clone()];

    let kimi_account = AccountConfig {
        name: "kimi-a".to_string(),
        ..AccountConfig::default()
    };
    config.providers.insert(
        "kimi-code".to_string(),
        ProviderConfig {
            kind: ProviderKind::Anthropic,
            base_url: "https://api.kimi.com/coding".to_string(),
            auth: AuthMode::KimiOauth,
            api_key_env: None,
            api_key_header: ApiKeyHeader::Bearer,
            effort: None,
            service_tier: None,
            count_tokens: CountTokens::default(),
            accounts: vec![kimi_account.clone()],
            account_scope: Vec::new(),
            websocket: false,
            tool_search: None,
            request_compression: true,
            retry: Default::default(),
            workspace_roots: Vec::new(),
            sandbox: true,
        },
    );

    let state = AppState::new(config, reqwest::Client::new()).unwrap();
    let seeded = |account: &AccountConfig, family: StoreFamily| AccountConfig {
        store_family: Some(family),
        ..account.clone()
    };
    state.accounts.note_usage(
        "codex",
        &seeded(&codex_account, StoreFamily::Chatgpt),
        &UsageSnapshot {
            five_hour: Some(UsageWindow {
                utilization: 0.25,
                resets_at: Some(reset_5h),
            }),
            seven_day: None,
            seven_day_oi: None,
        },
    );
    state.accounts.note_usage(
        "kimi-code",
        &seeded(&kimi_account, StoreFamily::Kimi),
        &UsageSnapshot {
            five_hour: Some(UsageWindow {
                utilization: 0.10,
                resets_at: Some(reset_5h),
            }),
            seven_day: None,
            seven_day_oi: None,
        },
    );

    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", "tok-secret".parse().unwrap());
    let response = get(State(state), headers).await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = body_json(response).await;
    std::env::remove_var(&env);

    assert_eq!(body["pool"]["windows"]["5h"]["remaining"], json!(0.90));
    assert_eq!(body["pool"]["windows"]["5h"]["resets_at"], json!(reset_5h));
}

#[tokio::test]
async fn rejects_a_request_without_a_valid_client_token() {
    let (state, env, _) = state_with_auth_and_seeded_pool("tok-secret", "rejects");
    // No credential header at all.
    let response = get(State(state), HeaderMap::new()).await;
    std::env::remove_var(&env);

    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    let body = body_json(response).await;
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "authentication_error");
}

#[tokio::test]
async fn fails_closed_when_inbound_auth_is_absent() {
    // Defense in depth for the branch config validation normally forbids:
    // with no `[server.auth]`, the handler must not serve pool telemetry.
    let state = AppState::new(crate::config::Config::default(), reqwest::Client::new()).unwrap();
    let response = get(State(state), HeaderMap::new()).await;
    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn returns_api_error_500_when_account_store_scan_fails() {
    use crate::auth::{codex::store as codex_store, shared::EnvVarGuard};

    // Serialize with the codex store's own env-var tests (they share the
    // `SHUNT_CODEX_ACCOUNTS_DIR` process env).
    let _guard = codex_store::TEST_ENV_LOCK.lock().await;
    // A file where the store expects a directory is a platform-stable way to
    // fail `fs::read_dir` (NotADirectory / ENOTDIR-equivalent) without racing
    // real filesystem permissions.
    let not_a_dir = std::env::temp_dir().join(format!(
        "shunt-usage-test-not-a-dir-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&not_a_dir, b"not a directory").unwrap();
    // Declared after TEST_ENV_LOCK so it drops first: the var is removed on
    // drop (panic-safe) while the lock is still held.
    let _env_dir = EnvVarGuard::set("SHUNT_CODEX_ACCOUNTS_DIR", &not_a_dir);

    // Default config: the built-in `codex` provider has no explicit accounts,
    // so the handler falls through to `scan_accounts`, which now fails.
    let env = format!(
        "SHUNT_USAGE_TEST_TOKENS_{}_store_scan_failure",
        std::process::id()
    );
    std::env::set_var(&env, "tester:tok-secret");
    let mut config = crate::config::Config::default();
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: env.clone(),
    });
    config.server.usage = Some(UsageEndpointConfig::default());
    let state = AppState::new(config, reqwest::Client::new()).unwrap();

    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", "tok-secret".parse().unwrap());
    let response = get(State(state), headers).await;
    std::env::remove_var(&env);
    let _ = std::fs::remove_file(&not_a_dir);

    assert_eq!(
        response.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
    let body = body_json(response).await;
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "api_error");
}
