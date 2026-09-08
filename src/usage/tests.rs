use axum::{extract::State, http::HeaderMap};
use serde_json::{json, Value};

use crate::{
    accounts::{AccountSnapshot, UsageSnapshot, UsageWindow},
    config::{AccountConfig, InboundAuthConfig, UsageEndpointConfig},
    server::AppState,
};

use super::{aggregate, aggregate_split, get};

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
        needs_relogin: false,
    }
}

#[test]
fn aggregate_reports_mean_headroom_and_earliest_reset_per_window() {
    // Two accounts at 0.60 and 0.25 → mean headroom 0.575; the earliest
    // reported reset (111) is when the aggregate can next change.
    let snapshots = vec![
        snapshot("acct-a", Some(0.60), Some(111), Some(0.40)),
        snapshot("acct-b", Some(0.25), Some(222), Some(0.90)),
    ];
    let body = serde_json::to_value(aggregate(&snapshots)).unwrap();
    assert_eq!(body["pool"]["status"], "ok");
    assert_eq!(body["pool"]["windows"]["5h"]["remaining"], json!(0.575));
    assert_eq!(body["pool"]["windows"]["5h"]["resets_at"], json!(111));
    // 7d: mean of 0.40 and 0.90 utilization → remaining 0.35.
    assert_eq!(body["pool"]["windows"]["7d"]["remaining"], json!(0.35));
    // No account reports the Fable window → null.
    assert_eq!(body["pool"]["windows"]["fable"]["remaining"], Value::Null);
    assert_eq!(body["pool"]["windows"]["fable"]["resets_at"], Value::Null);
}

#[test]
fn aggregate_counts_exhausted_accounts_against_pool_capacity() {
    // Nine exhausted accounts plus one fresh one leave a tenth of the pool's
    // capacity, not a whole pool (#482: the old `1 - min(utilization)` read
    // 1.0 here). Accounts not reporting the window stay out of the mean.
    let mut snapshots: Vec<_> = (0..9)
        .map(|i| snapshot(&format!("spent-{i}"), Some(1.0), Some(500), Some(1.0)))
        .collect();
    snapshots.push(snapshot("fresh", Some(0.0), None, None));
    let body = serde_json::to_value(aggregate(&snapshots)).unwrap();
    assert_eq!(body["pool"]["windows"]["5h"]["remaining"], json!(0.1));
    assert_eq!(body["pool"]["windows"]["5h"]["resets_at"], json!(500));
    assert_eq!(body["pool"]["windows"]["7d"]["remaining"], json!(0.0));
}

#[test]
fn aggregate_split_reads_status_from_every_row_and_windows_from_representatives() {
    // An alias flagged near-quota (its own threshold) still degrades `status`
    // even though only the representative's row feeds the mean.
    let representative = snapshot("acct", Some(0.20), Some(50), None);
    let mut alias = snapshot("acct-alias", Some(0.20), Some(50), None);
    alias.near_quota = true;
    let all = vec![representative.clone(), alias];
    let body = serde_json::to_value(aggregate_split(&all, &[representative])).unwrap();
    assert_eq!(body["pool"]["status"], "degraded");
    assert_eq!(body["pool"]["windows"]["5h"]["remaining"], json!(0.8));
    // The same rows fed to both sides give the alias a second vote.
    let both = serde_json::to_value(aggregate(&all)).unwrap();
    assert_eq!(both["pool"]["windows"]["5h"]["remaining"], json!(0.8));
    assert_eq!(both["pool"]["status"], "degraded");
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
/// that shifts the reported headroom: codex at 0.25 and Kimi at 0.10 average
/// to 0.825 remaining; were Kimi filtered out, 5h remaining would fall back to
/// codex's 0.75. Both accounts are seeded with the
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

    assert_eq!(body["pool"]["windows"]["5h"]["remaining"], json!(0.825));
    assert_eq!(body["pool"]["windows"]["5h"]["resets_at"], json!(reset_5h));
}

/// Config entries sharing a `uuid` are one physical account to the pool
/// (`collapse_representatives`), and `AccountPool::snapshot` emits one row per
/// entry, so without collapsing them the mean would give that subscription
/// extra votes. The fresh identity is configured three times here — twice on
/// the built-in `codex` provider and once more on a second `chatgpt_oauth`
/// provider (the key carries no provider name) — plus one exhausted identity:
/// uncollapsed that reads `0.75`, collapsed it reads the pool-capacity `0.5`.
/// (A repeated `account_scope` store reference yields two rows with one name;
/// config validation rejects that for explicit entries, so it is not built
/// here — representatives are matched by row position, which covers it.)
#[tokio::test]
async fn aggregate_counts_an_aliased_identity_once() {
    use crate::accounts::StoreFamily;

    let env = format!("SHUNT_USAGE_TEST_TOKENS_{}_alias", std::process::id());
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
    let fresh = AccountConfig {
        name: "fresh".to_string(),
        uuid: Some("shared-identity".to_string()),
        ..AccountConfig::default()
    };
    let fresh_alias = AccountConfig {
        name: "fresh-alias".to_string(),
        ..fresh.clone()
    };
    let spent = AccountConfig {
        name: "spent".to_string(),
        uuid: Some("other-identity".to_string()),
        ..AccountConfig::default()
    };
    config
        .providers
        .get_mut("codex")
        .expect("built-in codex provider")
        .accounts = vec![fresh.clone(), fresh_alias, spent.clone()];
    let mut second = config.providers["codex"].clone();
    second.accounts = vec![AccountConfig {
        name: "fresh-elsewhere".to_string(),
        ..fresh.clone()
    }];
    config.providers.insert("codex-2".to_string(), second);
    let state = AppState::new(config, reqwest::Client::new()).unwrap();
    let seeded = |account: &AccountConfig| AccountConfig {
        store_family: Some(StoreFamily::Chatgpt),
        ..account.clone()
    };
    for (account, utilization) in [(&fresh, 0.0), (&spent, 1.0)] {
        state.accounts.note_usage(
            "codex",
            &seeded(account),
            &UsageSnapshot {
                five_hour: Some(UsageWindow {
                    utilization,
                    resets_at: Some(reset_5h),
                }),
                seven_day: None,
                seven_day_oi: None,
            },
        );
    }

    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", "tok-secret".parse().unwrap());
    let response = get(State(state), headers).await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = body_json(response).await;
    std::env::remove_var(&env);

    assert_eq!(body["pool"]["windows"]["5h"]["remaining"], json!(0.5));
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
