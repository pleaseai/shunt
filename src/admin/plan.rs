//! Per-account subscription plan resolution for `GET /admin/pool`.
//!
//! The dashboard's pool table shows raw quota utilization but not which
//! subscription tier an account is on (Claude "max" vs "max 20x", ChatGPT
//! "team" vs "plus") — operators otherwise have to cross-reference the
//! provider's own billing page to know why one account's headroom differs
//! from another's. This module derives that label from data shunt already
//! holds or can cheaply fetch, and is purely informational: nothing here
//! touches [`crate::accounts::AccountSnapshot`] or the hot inference-routing
//! path (`crate::proxy`/`crate::routing`), so a plan-lookup failure can never
//! affect request handling — every error degrades to "no plan".
//!
//! Three sources, cheapest first:
//!   1. The account's own credential file, already on disk
//!      ([`crate::auth::shared::claude_plan_from_credentials`] /
//!      [`crate::auth::shared::codex_plan_from_credentials`] — shared with
//!      [`crate::auth::observation`] so the extraction logic lives once).
//!   2. For a Claude account whose credential file carries no
//!      `subscriptionType` (a `claude setup-token` login, or a very fresh
//!      device pairing), a live `GET {base_url}/api/oauth/profile` call
//!      ([`claude_plan_from_profile`]), cached for the process lifetime.
//!      The dashboard renders the pool once per page load, not on a periodic
//!      timer — the cache instead bounds repeat cost across an operator's
//!      page loads and any direct API client that hits this endpoint. The
//!      backfill attempt itself is time-boxed by [`BackfillBudgets`], so a
//!      stalled Claude endpoint can never make `GET /admin/pool` hang.
//!   3. Otherwise, absent — the pool response simply omits the `plan` key
//!      for that account.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use serde_json::Value;

use crate::{
    accounts::{account_key, AccountKey},
    auth::{
        self,
        claude::store as claude_store,
        codex::store as codex_store,
        shared::{claude_plan_from_credentials, codex_plan_from_credentials},
        Credential,
    },
    config::{AccountConfig, AuthMode},
};

/// Derive a plan label from a Claude `GET /api/oauth/profile` response, used
/// to backfill accounts whose credential file carries no `subscriptionType`.
/// Two source fields, most specific first:
///   - `organization.rate_limit_tier`, e.g. `"default_claude_max_20x"` ->
///     `"max 20x"`. Anthropic's own naming for the per-seat multiplier tiers
///     (`5x`, `20x`, …) sold above the base "max" plan.
///   - `organization.organization_type`, e.g. `"claude_max"` -> `"max"`, the
///     coarser field present even without a multiplier tier.
///
/// An unrecognized shape (neither field present, or a `rate_limit_tier` that
/// doesn't match the `default_claude_{plan}_{N}x` pattern and no fallback
/// `organization_type`) yields `None` rather than guessing — this is a
/// display label, and a wrong guess is worse than a blank cell.
pub(crate) fn claude_plan_from_profile(value: &Value) -> Option<String> {
    let organization = value.get("organization")?;
    if let Some(tier) = organization.get("rate_limit_tier").and_then(Value::as_str) {
        if let Some(plan) = parse_rate_limit_tier(tier) {
            return Some(plan);
        }
    }
    organization
        .get("organization_type")
        .and_then(Value::as_str)
        .and_then(parse_organization_type)
}

/// `"default_claude_max_20x"` -> `Some("max 20x")`. Requires the
/// `default_claude_` prefix and a trailing `_<digits>x` multiplier; anything
/// else (e.g. a tier with no multiplier suffix) returns `None` so the caller
/// falls back to `organization_type` instead of emitting a mangled label.
fn parse_rate_limit_tier(tier: &str) -> Option<String> {
    let rest = tier.strip_prefix("default_claude_")?;
    let (plan, multiplier) = rest.rsplit_once('_')?;
    if plan.is_empty() {
        return None;
    }
    let digits = multiplier.strip_suffix('x')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("{plan} {multiplier}"))
}

/// `"claude_max"` -> `Some("max")`.
fn parse_organization_type(organization_type: &str) -> Option<String> {
    let plan = organization_type.strip_prefix("claude_")?;
    (!plan.is_empty()).then(|| plan.to_string())
}

/// Time budget for the Claude profile backfill step. The three fields are
/// always injected together as this one struct — never add a function that
/// accepts just one of them, since `min_slice` only makes sense relative to
/// `total` and `per_account`. The production/operational code path
/// (`admin::pool`) uses only [`BackfillBudgets::default`]; the non-default
/// constructor exists so a test can shrink the budget far below production
/// scale without waiting on a real multi-second stall.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BackfillBudgets {
    /// Upper bound on the whole backfill step across every provider handled
    /// by one `/admin/pool` request. Computed once, before the per-provider
    /// loop begins, and shared by every provider in that one request — a
    /// stalled account in an earlier provider must not let a later provider
    /// spend its own separate budget on top.
    pub(crate) total: Duration,
    /// Upper bound on one account's resolve-plus-fetch attempt.
    pub(crate) per_account: Duration,
    /// Below this much remaining budget, skip an account's attempt rather
    /// than start one almost certain to be cut off mid-flight.
    pub(crate) min_slice: Duration,
}

impl Default for BackfillBudgets {
    fn default() -> Self {
        Self {
            total: Duration::from_secs(8),
            per_account: Duration::from_secs(5),
            min_slice: Duration::from_secs(2),
        }
    }
}

/// Resolve a `name -> plan` map for one provider's pool accounts. `auth` is
/// the account family's actual auth kind (mirrors the pool handler's own
/// `provider.auth`, not the operator's free-form provider table name);
/// `upstream` is that provider table name, used only to key the profile
/// cache ([`crate::accounts::account_key`]). `base_url` and `client` back
/// the Claude profile backfill, bounded by `budgets` and the shared
/// `deadline` (computed once by the caller — see `admin::pool` — so multiple
/// providers in one request share a single total time budget rather than
/// each getting their own). Unsupported families (Kimi has no known
/// subscription-plan concept in its credential shape) return an empty map
/// rather than erroring.
pub(crate) async fn plans_for_accounts(
    auth: AuthMode,
    upstream: &str,
    base_url: &str,
    client: &reqwest::Client,
    accounts: &[AccountConfig],
    budgets: &BackfillBudgets,
    deadline: tokio::time::Instant,
) -> HashMap<String, String> {
    let mut plans = file_derived_plans(auth, accounts).await;
    if matches!(auth, AuthMode::ClaudeOauth) {
        backfill_claude_profile_plans(
            upstream, base_url, client, accounts, budgets, deadline, &mut plans,
        )
        .await;
    }
    plans
}

/// Path to an account's own credential file: the store path when it is a
/// store entry, otherwise its configured `credentials` path (already
/// tilde-expanded at config parse — see `config::AccountConfig`). A
/// `token_env`-only account has no file to read and is skipped.
fn credential_path(auth: AuthMode, account: &AccountConfig) -> Option<PathBuf> {
    if account.store_entry {
        return Some(match auth {
            AuthMode::ChatgptOauth => codex_store::account_path(&account.name),
            _ => claude_store::account_path(&account.name),
        });
    }
    account.credentials.as_deref().map(PathBuf::from)
}

/// Read every resolvable credential file and extract a plan from each, in one
/// batched `spawn_blocking` call (never one task per account — a pool can hold
/// dozens of accounts, and each is a synchronous file read that must not run
/// on a runtime worker thread). Kimi (and any other unsupported family) short
/// circuits before touching the filesystem at all.
async fn file_derived_plans(auth: AuthMode, accounts: &[AccountConfig]) -> HashMap<String, String> {
    let extractor: fn(&Value) -> Option<String> = match auth {
        AuthMode::ClaudeOauth => claude_plan_from_credentials,
        AuthMode::ChatgptOauth => codex_plan_from_credentials,
        _ => return HashMap::new(),
    };
    let candidates: Vec<(String, PathBuf)> = accounts
        .iter()
        .filter_map(|account| {
            credential_path(auth, account).map(|path| (account.name.clone(), path))
        })
        .collect();
    if candidates.is_empty() {
        return HashMap::new();
    }
    tokio::task::spawn_blocking(move || {
        candidates
            .into_iter()
            .filter_map(|(name, path)| {
                let value = read_json(&path)?;
                extractor(&value).map(|plan| (name, plan))
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

fn read_json(path: &PathBuf) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// A resolved (or attempted-and-failed) Claude profile plan lookup, cached for
/// the process lifetime. Success is cached far longer than failure: a plan
/// tier changes rarely (an operator upgrading their subscription), while a
/// failure (network blip, token not yet refreshable) is worth retrying soon.
struct CachedProfilePlan {
    plan: Option<String>,
    fetched_at: Instant,
}

impl CachedProfilePlan {
    fn ttl(&self) -> Duration {
        if self.plan.is_some() {
            Duration::from_secs(24 * 60 * 60)
        } else {
            Duration::from_secs(10 * 60)
        }
    }

    fn is_stale(&self) -> bool {
        self.fetched_at.elapsed() >= self.ttl()
    }
}

/// Keyed by [`AccountKey`] — the pool's own stable-identity scheme, shared
/// with `crate::accounts` and the pool health map — rather than by
/// credential content, so a token refresh never invalidates the cache entry.
/// The admin pool handler always routes accounts through
/// `crate::auth::shared::resolve_pool_accounts` first, so `account_key`'s
/// name-guessing `UpstreamInline` fallback is never actually exercised here:
/// every account arrives either `Verified` (a known uuid) or `StoreEntry`.
fn profile_cache() -> &'static Mutex<HashMap<AccountKey, CachedProfilePlan>> {
    static CACHE: OnceLock<Mutex<HashMap<AccountKey, CachedProfilePlan>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_profile_plan(key: &AccountKey) -> Option<Option<String>> {
    let cache = profile_cache().lock().expect("plan cache lock poisoned");
    cache
        .get(key)
        .filter(|entry| !entry.is_stale())
        .map(|entry| entry.plan.clone())
}

fn store_profile_plan(key: AccountKey, plan: Option<String>) {
    let mut cache = profile_cache().lock().expect("plan cache lock poisoned");
    cache.insert(
        key,
        CachedProfilePlan {
            plan,
            fetched_at: Instant::now(),
        },
    );
}

/// Clear the process-wide profile cache. Exposed for the integration test
/// target only (`tests/admin_surface.rs`), not a stability commitment —
/// mirrors the existing `#[doc(hidden)] pub fn` pattern used elsewhere in
/// this crate to reach across the same kind of compilation-unit boundary for
/// the benchmark target (see `crate::adapters::cursor::decode_selected_images`
/// and `crate::adapters::cursor::agent::build_run_frames`). A plain
/// `#[cfg(test)]` item does not work here: `tests/*.rs` files compile as a
/// separate crate linking only this library's normal (non-test) build, so a
/// `cfg(test)`-gated item stays invisible to them even though it is visible
/// to this crate's own unit tests below.
///
/// The profile cache and [`BACKFILL_LOCK`] are process-wide statics shared by
/// every test in one test binary process; a test that exercises the backfill
/// path must call this at its own start rather than risk inheriting an entry
/// a different test's account happened to leave behind.
#[doc(hidden)]
pub fn reset_profile_cache() {
    profile_cache()
        .lock()
        .expect("plan cache lock poisoned")
        .clear();
}

/// In-process single-flight for the Claude profile backfill step, mirroring
/// [`crate::auth::claude::auth`]'s `REFRESH_LOCK`: concurrent `/admin/pool`
/// requests must not each spend their own budget re-attempting the same
/// stalled accounts, and a waiter that acquires the lock after another
/// caller already finished should reuse what that caller cached rather than
/// repeat the attempt.
static BACKFILL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// For every `claude_oauth` account whose file-derived plan is unknown,
/// backfill it from a live profile fetch — but only for a refreshable
/// imported login (mirrors [`crate::usage_poll::account_is_refreshable`]'s
/// eligibility rule: a `claude setup-token` cannot call this endpoint
/// either). Every failure (ineligible credential, network error, non-2xx,
/// unparseable body, or a timeout against `budgets`/`deadline`) degrades to
/// "no plan" rather than propagating — this field is purely informational,
/// so it must never turn a working pool dashboard into a broken one.
///
/// Flow: (1) an account with a file-derived plan is skipped outright; (2) a
/// profile-cache hit is harvested regardless of remaining budget — reading
/// the cache is free; (3) every other account becomes an attempt candidate.
/// With no candidates, this returns immediately without ever touching
/// [`BACKFILL_LOCK`]. Otherwise the lock is acquired with a
/// `deadline`-bounded `timeout_at` — on timeout, only what step (2) already
/// harvested is kept. Once inside the lock, each candidate is re-checked
/// against the cache (another concurrent caller may have resolved it while
/// this caller waited — the same double-check `crate::auth::claude::auth`'s
/// `get_valid_access_token` performs on `REFRESH_LOCK`, see
/// `src/auth/claude/auth.rs:123-129`), then attempted with
/// `tokio::time::timeout(min(budgets.per_account, remaining), ..)`. A
/// candidate whose remaining budget has already dropped below
/// `budgets.min_slice` is skipped without an attempt and without a cache
/// write — a future request with a fresh budget should still get a real
/// attempt, not inherit a false "no plan" left by a starved one.
async fn backfill_claude_profile_plans(
    upstream: &str,
    base_url: &str,
    client: &reqwest::Client,
    accounts: &[AccountConfig],
    budgets: &BackfillBudgets,
    deadline: tokio::time::Instant,
    plans: &mut HashMap<String, String>,
) {
    let mut candidates: Vec<(&AccountConfig, AccountKey)> = Vec::new();
    for account in accounts {
        if plans.contains_key(&account.name) {
            continue;
        }
        let key = account_key(upstream, account);
        if let Some(cached) = cached_profile_plan(&key) {
            if let Some(plan) = cached {
                plans.insert(account.name.clone(), plan);
            }
            continue;
        }
        candidates.push((account, key));
    }
    if candidates.is_empty() {
        return;
    }

    let Ok(_guard) = tokio::time::timeout_at(deadline, BACKFILL_LOCK.lock()).await else {
        tracing::debug!(
            provider = upstream,
            candidates = candidates.len(),
            "admin pool: profile backfill lock wait timed out, returning cache-only plans"
        );
        return;
    };

    for (account, key) in candidates {
        // Double-check: another caller may have resolved and cached this
        // exact account while this caller waited for the lock.
        if let Some(cached) = cached_profile_plan(&key) {
            if let Some(plan) = cached {
                plans.insert(account.name.clone(), plan);
            }
            continue;
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining < budgets.min_slice {
            continue;
        }

        if !crate::usage_poll::account_is_refreshable(account).await {
            store_profile_plan(key, None);
            continue;
        }

        let attempt_budget = budgets.per_account.min(remaining);
        match tokio::time::timeout(
            attempt_budget,
            fetch_claude_profile_plan(base_url, client, account),
        )
        .await
        {
            Ok(Some(plan)) => {
                store_profile_plan(key, Some(plan.clone()));
                plans.insert(account.name.clone(), plan);
            }
            Ok(None) => {
                tracing::debug!(
                    provider = upstream,
                    account = %account.name,
                    "admin pool: profile backfill fetch failed"
                );
                store_profile_plan(key, None);
            }
            Err(_) => {
                tracing::debug!(
                    provider = upstream,
                    account = %account.name,
                    timeout_ms = attempt_budget.as_millis() as u64,
                    "admin pool: profile backfill attempt timed out"
                );
                store_profile_plan(key, None);
            }
        }
    }
}

/// One `GET {base_url}/api/oauth/profile` call, mirroring the OAuth bearer +
/// header pattern `claude::usage::fetch_usage` already uses against the
/// adjacent `/api/oauth/usage` endpoint. The caller wraps this whole call
/// (including the `resolve_claude_account` token resolution, which can itself
/// make an unbounded network call to refresh an expired token) in an outer
/// `tokio::time::timeout`; the `reqwest` timeout below is kept as a secondary
/// defense-in-depth layer, not the primary bound.
async fn fetch_claude_profile_plan(
    base_url: &str,
    client: &reqwest::Client,
    account: &AccountConfig,
) -> Option<String> {
    let credential = auth::resolve_claude_account(account, client).await.ok()?;
    let Credential::ClaudeOauth { access_token, .. } = credential else {
        return None;
    };
    let url = format!("{}/api/oauth/profile", base_url.trim_end_matches('/'));
    let response = client
        .get(&url)
        .header("authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("anthropic-version", "2023-06-01")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let value: Value = response.json().await.ok()?;
    claude_plan_from_profile(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_plan_from_profile_prefers_rate_limit_tier() {
        let value = serde_json::json!({"organization": {
            "rate_limit_tier": "default_claude_max_20x",
            "organization_type": "claude_max"
        }});
        assert_eq!(claude_plan_from_profile(&value).as_deref(), Some("max 20x"));
    }

    #[test]
    fn claude_plan_from_profile_falls_back_to_organization_type() {
        let no_tier = serde_json::json!({"organization": {"organization_type": "claude_max"}});
        assert_eq!(claude_plan_from_profile(&no_tier).as_deref(), Some("max"));

        let unmatched_tier = serde_json::json!({"organization": {
            "rate_limit_tier": "some_other_shape",
            "organization_type": "claude_pro"
        }});
        assert_eq!(
            claude_plan_from_profile(&unmatched_tier).as_deref(),
            Some("pro")
        );
    }

    #[test]
    fn claude_plan_from_profile_absent_or_unrecognized_is_none() {
        assert_eq!(claude_plan_from_profile(&serde_json::json!({})), None);
        assert_eq!(
            claude_plan_from_profile(&serde_json::json!({"organization": {}})),
            None
        );
        assert_eq!(
            claude_plan_from_profile(
                &serde_json::json!({"organization": {"organization_type": "unexpected"}})
            ),
            None
        );
    }

    #[test]
    fn parse_rate_limit_tier_requires_digit_x_multiplier() {
        assert_eq!(
            parse_rate_limit_tier("default_claude_max_20x").as_deref(),
            Some("max 20x")
        );
        assert_eq!(parse_rate_limit_tier("default_claude_max"), None);
        assert_eq!(parse_rate_limit_tier("default_claude_max_xx"), None);
        assert_eq!(parse_rate_limit_tier("not_the_expected_prefix"), None);
    }

    #[tokio::test]
    async fn file_derived_plans_reads_claude_credentials_and_skips_token_env_only() {
        let dir = std::env::temp_dir().join(format!(
            "shunt-plan-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let with_plan_path = dir.join("with-plan.json");
        std::fs::write(
            &with_plan_path,
            serde_json::json!({"claudeAiOauth": {"subscriptionType": "max"}}).to_string(),
        )
        .unwrap();

        let accounts = vec![
            AccountConfig {
                name: "with-plan".to_string(),
                credentials: Some(with_plan_path.to_string_lossy().into_owned()),
                ..Default::default()
            },
            AccountConfig {
                name: "env-only".to_string(),
                token_env: Some("SHUNT_PLAN_TEST_UNUSED_ENV".to_string()),
                ..Default::default()
            },
        ];

        let plans = file_derived_plans(AuthMode::ClaudeOauth, &accounts).await;
        assert_eq!(plans.get("with-plan").map(String::as_str), Some("max"));
        assert_eq!(plans.get("env-only"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn file_derived_plans_empty_for_unsupported_family() {
        let accounts = vec![AccountConfig {
            name: "kimi-account".to_string(),
            credentials: Some("/does/not/matter".to_string()),
            ..Default::default()
        }];
        let plans = file_derived_plans(AuthMode::KimiOauth, &accounts).await;
        assert!(plans.is_empty());
    }

    /// A refreshable-login fixture for the backfill tests below: a non-empty
    /// `refreshToken` and an `expiresAt` far in the future (so
    /// `resolve_claude_account` resolves the access token straight from disk,
    /// no token-endpoint round trip needed) and no `subscriptionType` (so
    /// `file_derived_plans` yields nothing and the profile backfill path is
    /// the only source).
    fn write_refreshable_fixture(dir: &std::path::Path, name: &str) -> PathBuf {
        let expires_at_ms = (std::time::SystemTime::now() + Duration::from_secs(3600))
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let path = dir.join(format!("{name}.json"));
        std::fs::write(
            &path,
            serde_json::json!({"claudeAiOauth": {
                "accessToken": "access-token",
                "refreshToken": "refresh-token",
                "expiresAt": expires_at_ms
            }})
            .to_string(),
        )
        .unwrap();
        path
    }

    fn unique_test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shunt-plan-backfill-test-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[tokio::test]
    async fn backfill_bounds_total_time_when_profile_endpoint_stalls() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        reset_profile_cache();
        let dir = unique_test_dir("stall");
        std::fs::create_dir_all(&dir).unwrap();
        let creds_path = write_refreshable_fixture(&dir, "stalled-account");
        let account = AccountConfig {
            name: "stalled-account".to_string(),
            uuid: Some("stalled-account-uuid".to_string()),
            credentials: Some(creds_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/profile"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(5))
                    .set_body_json(
                        serde_json::json!({"organization": {"organization_type": "claude_max"}}),
                    ),
            )
            .mount(&server)
            .await;

        let budgets = BackfillBudgets {
            total: Duration::from_millis(800),
            per_account: Duration::from_millis(300),
            min_slice: Duration::from_millis(50),
        };
        let deadline = tokio::time::Instant::now() + budgets.total;
        let client = reqwest::Client::new();

        let started = Instant::now();
        let plans = plans_for_accounts(
            AuthMode::ClaudeOauth,
            "stall-test-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &budgets,
            deadline,
        )
        .await;
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(1),
            "plans_for_accounts took {elapsed:?}, expected under 1s given an 800ms total budget"
        );
        assert!(
            !plans.contains_key("stalled-account"),
            "a stalled endpoint must never resolve a plan"
        );
        assert!(
            !server.received_requests().await.unwrap().is_empty(),
            "the attempt must have actually reached the mock, not been skipped for an unrelated reason"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn backfill_skip_from_starved_budget_does_not_cache_a_false_failure() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        reset_profile_cache();
        let dir = unique_test_dir("starved");
        std::fs::create_dir_all(&dir).unwrap();
        let creds_path = write_refreshable_fixture(&dir, "starved-account");
        let account = AccountConfig {
            name: "starved-account".to_string(),
            uuid: Some("starved-account-uuid".to_string()),
            credentials: Some(creds_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/profile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"organization": {"organization_type": "claude_max"}}),
            ))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();

        // Negative case: min_slice exceeds the total budget, so the single
        // candidate is skipped before ever attempting.
        let starved_budgets = BackfillBudgets {
            total: Duration::from_millis(100),
            per_account: Duration::from_secs(5),
            min_slice: Duration::from_secs(2),
        };
        let deadline = tokio::time::Instant::now() + starved_budgets.total;
        let plans = plans_for_accounts(
            AuthMode::ClaudeOauth,
            "starved-test-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &starved_budgets,
            deadline,
        )
        .await;
        assert!(!plans.contains_key("starved-account"));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            0,
            "a candidate skipped for insufficient remaining budget must never attempt"
        );

        // Recovery: same account (same upstream string, so the same
        // AccountKey), normal budgets, no mock delay -- must resolve for
        // real. If the starved attempt above had wrongly cached a 10-minute
        // failure, this call would incorrectly skip too and this assertion
        // would fail.
        let normal_budgets = BackfillBudgets::default();
        let deadline = tokio::time::Instant::now() + normal_budgets.total;
        let plans = plans_for_accounts(
            AuthMode::ClaudeOauth,
            "starved-test-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &normal_budgets,
            deadline,
        )
        .await;
        assert_eq!(
            plans.get("starved-account").map(String::as_str),
            Some("max"),
            "a starved skip must not poison the cache with a false failure"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn backfill_single_flights_concurrent_calls_for_the_same_account() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        reset_profile_cache();
        let dir = unique_test_dir("concurrent");
        std::fs::create_dir_all(&dir).unwrap();
        let creds_path = write_refreshable_fixture(&dir, "concurrent-account");
        let account = AccountConfig {
            name: "concurrent-account".to_string(),
            uuid: Some("concurrent-account-uuid".to_string()),
            credentials: Some(creds_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/profile"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(200))
                    .set_body_json(
                        serde_json::json!({"organization": {"organization_type": "claude_max"}}),
                    ),
            )
            .mount(&server)
            .await;

        let budgets = BackfillBudgets::default();
        let deadline = tokio::time::Instant::now() + budgets.total;
        let client = reqwest::Client::new();
        let server_uri = server.uri();

        let (a, b) = tokio::join!(
            plans_for_accounts(
                AuthMode::ClaudeOauth,
                "concurrent-test-upstream",
                &server_uri,
                &client,
                std::slice::from_ref(&account),
                &budgets,
                deadline,
            ),
            plans_for_accounts(
                AuthMode::ClaudeOauth,
                "concurrent-test-upstream",
                &server_uri,
                &client,
                std::slice::from_ref(&account),
                &budgets,
                deadline,
            ),
        );
        assert_eq!(a.get("concurrent-account").map(String::as_str), Some("max"));
        assert_eq!(b.get("concurrent-account").map(String::as_str), Some("max"));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "two concurrent callers for the same account must single-flight to one request"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
