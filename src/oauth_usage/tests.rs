use axum::{extract::State, http::HeaderMap};
use serde_json::{json, Value};

use crate::{
    accounts::{AccountSnapshot, UsageSnapshot, UsageWindow},
    config::{AccountConfig, AuthMode, InboundAuthConfig, OauthUsageConfig},
    server::AppState,
};

use super::{get, routing_aware_window, to_wire};

/// A seen account snapshot with the given per-window utilization and
/// priority/availability; all other fields default to a non-disabled account.
fn snapshot(
    name: &str,
    priority: u32,
    available: bool,
    util_5h: Option<f64>,
    reset_5h: Option<u64>,
) -> AccountSnapshot {
    AccountSnapshot {
        name: name.to_string(),
        has_state: true,
        available,
        near_quota: false,
        cooldown_secs_remaining: None,
        priority,
        disabled: false,
        headroom_secs: None,
        utilization_5h: util_5h,
        reset_5h,
        utilization_7d: None,
        reset_7d: None,
        utilization_7d_oi: None,
        reset_7d_oi: None,
        status: None,
    }
}

#[test]
fn routing_aware_window_returns_none_when_no_account_reports_it() {
    let snapshots = [snapshot("a", 100, true, None, None)];
    assert_eq!(
        routing_aware_window(&snapshots, |s| s.utilization_5h, |s| s.reset_5h),
        None
    );
}

#[test]
fn to_wire_omits_absent_windows_and_fable_limit() {
    let snapshots = [snapshot("a", 100, true, None, None)];
    let wire = to_wire(&snapshots);
    let body = serde_json::to_value(&wire).unwrap();
    assert!(body.get("five_hour").is_none());
    assert!(body.get("seven_day").is_none());
    assert!(body.get("limits").is_none());
}

#[test]
fn to_wire_rounds_percent_to_two_decimals() {
    let snapshots = [snapshot(
        "a",
        100,
        true,
        Some(0.423_712),
        Some(1_800_000_000),
    )];
    let wire = to_wire(&snapshots);
    let body = serde_json::to_value(&wire).unwrap();
    assert_eq!(body["five_hour"]["utilization"], json!(42.37));
    assert_eq!(
        body["five_hour"]["resets_at"],
        json!("2027-01-15T08:00:00Z")
    );
}

/// Direct regression test for the failure mode that motivated Deviation 2: a
/// priority-1 (preferred) account at 95% utilization plus a priority-100
/// (backup) account at 5% must report the priority-1 account's 95%, not the
/// 5% a pool-wide least-utilized aggregate would have reported.
#[test]
fn priority_tiered_worst_case_reflects_the_preferred_tier_not_the_least_utilized() {
    let snapshots = [
        snapshot("preferred", 1, true, Some(0.95), Some(111)),
        snapshot("backup", 100, true, Some(0.05), Some(222)),
    ];
    let (used, resets_at) =
        routing_aware_window(&snapshots, |s| s.utilization_5h, |s| s.reset_5h).unwrap();
    assert_eq!(used, 0.95);
    assert_eq!(resets_at, Some(111));
}

/// When every non-disabled account is unavailable (cooling/near-quota), the
/// function still returns a value by falling back to the full non-disabled
/// set (step 3 of the algorithm), rather than `None`/omitted, for a pool that
/// is in practice still routable-to, just degraded.
#[test]
fn falls_back_to_full_set_when_no_account_is_available() {
    let snapshots = [
        snapshot("preferred-cooling", 1, false, Some(0.99), Some(111)),
        snapshot("backup-cooling", 100, false, Some(0.10), Some(222)),
    ];
    let (used, resets_at) =
        routing_aware_window(&snapshots, |s| s.utilization_5h, |s| s.reset_5h).unwrap();
    // Falls back to the full non-disabled set; the priority-1 (the
    // lowest-priority value present in that fallback set) governs.
    assert_eq!(used, 0.99);
    assert_eq!(resets_at, Some(111));
}

/// Config with `[server.auth]` bound to a unique env var and one explicit
/// `ClaudeOauth` account on the built-in `anthropic` provider, so the
/// snapshot path does not touch the account store.
fn state_with_claude_account(label: &str, bind: &str) -> (AppState, String) {
    state_with_claude_and_codex_accounts(label, bind, None)
}

/// Like [`state_with_claude_account`], but when `codex_account` is `Some`,
/// also gives the built-in `codex` (`ChatgptOauth`) provider one explicit
/// account of that name — instead of the default empty `accounts: []`, which
/// would fall back to scanning the real on-disk Claude account store and so
/// could never deterministically contain a given test account name. Callers
/// that need to prove the handler's Claude-only provider filter (not just an
/// accidental account-identity mismatch) need this explicit account so a
/// deleted filter would actually change the response.
fn state_with_claude_and_codex_accounts(
    label: &str,
    bind: &str,
    codex_account: Option<&str>,
) -> (AppState, String) {
    let env = format!(
        "SHUNT_OAUTH_USAGE_TEST_TOKENS_{}_{label}",
        std::process::id()
    );
    std::env::set_var(&env, "tester:tok-secret");
    let mut config = crate::config::Config::default();
    config.server.bind = bind.to_string();
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: env.clone(),
    });
    config.server.oauth_usage = Some(OauthUsageConfig::default());
    let anthropic = config.providers.get_mut("anthropic").unwrap();
    anthropic.auth = AuthMode::ClaudeOauth;
    anthropic.accounts = vec![AccountConfig {
        name: "acct-a".to_string(),
        ..AccountConfig::default()
    }];
    if let Some(codex_account) = codex_account {
        let codex = config
            .providers
            .get_mut("codex")
            .expect("default config always carries a codex provider");
        codex.accounts = vec![AccountConfig {
            name: codex_account.to_string(),
            ..AccountConfig::default()
        }];
    }
    let state = AppState::new(config, reqwest::Client::new()).unwrap();
    (state, env)
}

async fn body_json(response: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// An hour from now, in unix seconds — `AccountPool::snapshot` expires any
/// quota window whose `resets_at` has already passed, so handler-level tests
/// that need a window to survive the snapshot must use a future reset.
fn future_reset_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600
}

#[tokio::test]
async fn handler_returns_empty_object_when_no_claude_oauth_provider_is_configured() {
    let state = AppState::new(crate::config::Config::default(), reqwest::Client::new()).unwrap();
    let response = get(State(state), HeaderMap::new()).await;
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body, json!({}));
}

#[tokio::test]
async fn sanitization_never_exposes_account_identity_or_capacity() {
    let (state, env) = state_with_claude_account("sanitization", "127.0.0.1:0");
    // `note_usage` keys its health entry by `account_identity` (name, absent a
    // uuid) — it must name the *same* account `state_with_claude_account`
    // configured (`acct-a`), otherwise `AccountPool::snapshot` never finds an
    // `observed` entry for the configured account and the response body is
    // `{}`, which would make the leak assertions below pass vacuously
    // whether or not sanitization actually works.
    let account = AccountConfig {
        name: "acct-a".to_string(),
        priority: 5,
        ..AccountConfig::default()
    };
    state.accounts.note_usage(
        "anthropic",
        &account,
        &UsageSnapshot {
            five_hour: Some(UsageWindow {
                utilization: 0.30,
                resets_at: Some(future_reset_secs()),
            }),
            seven_day: None,
            seven_day_oi: None,
        },
    );
    let response = get(State(state), HeaderMap::new()).await;
    std::env::remove_var(&env);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    // Prove the body actually carries the utilization we noted above — a
    // non-empty body is the precondition for the leak checks below to mean
    // anything.
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["five_hour"]["utilization"], json!(30.0));
    for leak in [
        "acct-a",
        "\"name\"",
        "\"priority\"",
        "\"disabled\"",
        "threshold",
        "headroom",
        "cooldown",
        "\"status\"",
    ] {
        assert!(!text.contains(leak), "leaked {leak:?}: {text}");
    }
}

/// Deviation 1 regression test: a `ChatgptOauth` (Codex) account's high
/// utilization must never leak into the Claude-only response.
///
/// Configures an *explicit* `codex-acct` account on the `codex`
/// (`ChatgptOauth`) provider — not the default empty `accounts: []`, which
/// falls back to scanning the real on-disk Claude account store. With an
/// empty list, deleting the `provider.auth != AuthMode::ClaudeOauth` filter
/// in the handler would not have flipped this test's result (the scan would
/// not turn up an account named `codex-acct` either way), so it would not
/// have caught that regression. An explicit matching account closes that
/// gap: only the provider-auth filter stands between the noted 99% and the
/// response.
#[tokio::test]
async fn only_claude_oauth_accounts_contribute_never_chatgpt_oauth() {
    let (state, env) =
        state_with_claude_and_codex_accounts("provider_filter", "127.0.0.1:0", Some("codex-acct"));
    state.accounts.note_usage(
        "anthropic",
        &AccountConfig {
            name: "acct-a".to_string(),
            ..AccountConfig::default()
        },
        &UsageSnapshot {
            five_hour: Some(UsageWindow {
                utilization: 0.10,
                resets_at: Some(future_reset_secs()),
            }),
            seven_day: None,
            seven_day_oi: None,
        },
    );
    state.accounts.note_usage(
        "codex",
        &AccountConfig {
            name: "codex-acct".to_string(),
            ..AccountConfig::default()
        },
        &UsageSnapshot {
            // Deliberately below `SWITCH_THRESHOLD` (0.98): at 0.99 the
            // account would land in `routing_aware_window`'s "near quota,
            // unavailable" bucket and get excluded from the worst-case tier
            // for that reason alone — passing this test either way, whether
            // or not the provider filter above actually runs. 0.60 stays in
            // the "available" tier so it competes for (and, if it leaked,
            // would win) the worst-case slot against the Claude account's
            // 10%, making the provider filter the only thing standing
            // between it and the response.
            five_hour: Some(UsageWindow {
                utilization: 0.60,
                resets_at: Some(future_reset_secs()),
            }),
            seven_day: None,
            seven_day_oi: None,
        },
    );

    let response = get(State(state), HeaderMap::new()).await;
    std::env::remove_var(&env);
    let body = body_json(response).await;
    // Only the Claude account's 10% shows up; the Codex account's 60% never does.
    assert_eq!(body["five_hour"]["utilization"], json!(10.0));
}

#[tokio::test]
async fn loopback_bind_serves_unauthenticated_even_with_server_auth_configured() {
    let (state, env) = state_with_claude_account("loopback_no_auth", "127.0.0.1:0");
    let response = get(State(state), HeaderMap::new()).await;
    std::env::remove_var(&env);
    assert_eq!(response.status(), axum::http::StatusCode::OK);
}

#[tokio::test]
async fn non_loopback_bind_admits_a_valid_client_token() {
    let (state, env) = state_with_claude_account("non_loopback_valid", "0.0.0.0:0");
    let mut headers = HeaderMap::new();
    // The helper configures `[server.auth]` header `x-shunt-token` with the
    // token `tok-secret` (client `tester`).
    headers.insert("x-shunt-token", "tok-secret".parse().unwrap());
    let response = get(State(state), headers).await;
    std::env::remove_var(&env);
    assert_eq!(response.status(), axum::http::StatusCode::OK);
}

#[tokio::test]
async fn non_loopback_bind_rejects_a_fabricated_non_matching_credential() {
    // A remote caller presenting a header that is *not* a configured client
    // token (nor a valid gateway JWT) must be rejected — bare presence is not
    // enough. Guards against scraping pool quota telemetry with a fabricated
    // `Authorization`/`x-api-key` header on a network-facing bind.
    let (state, env) = state_with_claude_account("non_loopback_fabricated", "0.0.0.0:0");
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        "Bearer not-a-configured-token".parse().unwrap(),
    );
    let response = get(State(state), headers).await;
    std::env::remove_var(&env);
    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    let body = body_json(response).await;
    assert_eq!(body["error"]["type"], "authentication_error");
}

#[tokio::test]
async fn non_loopback_bind_rejects_when_no_credential_is_presented_at_all() {
    let (state, env) = state_with_claude_account("non_loopback_missing", "0.0.0.0:0");
    let response = get(State(state), HeaderMap::new()).await;
    std::env::remove_var(&env);
    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    let body = body_json(response).await;
    assert_eq!(body["error"]["type"], "authentication_error");
}
