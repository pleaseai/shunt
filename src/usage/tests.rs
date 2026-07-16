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
    config
        .providers
        .get_mut("codex")
        .expect("built-in codex provider")
        .accounts = vec![AccountConfig {
        name: "acct-a".to_string(),
        ..AccountConfig::default()
    }];
    let state = AppState::new(config, reqwest::Client::new()).unwrap();
    // Seed authoritative usage for the codex account (in production the Codex
    // backend has no quota headers; here we seed it to exercise the flow).
    state.accounts.note_usage(
        "codex",
        "acct-a",
        &UsageSnapshot {
            five_hour: Some(UsageWindow {
                utilization: 0.25,
                resets_at: Some(reset_5h),
            }),
            seven_day: Some(UsageWindow {
                utilization: 0.40,
                resets_at: Some(reset_5h + 3_600),
            }),
            seven_day_oi: None,
        },
    );
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
    assert_eq!(body["pool"]["windows"]["fable"]["remaining"], Value::Null);
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
