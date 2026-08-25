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
//!   2. For any Claude account carrying a still-valid access token
//!      ([`crate::auth::claude::auth::refreshable_valid_access_token`], read
//!      from the same file the first source already opened), a live
//!      `GET {base_url}/api/oauth/profile` call ([`claude_plan_from_profile`]),
//!      cached for the process lifetime. This runs even when the file
//!      already yielded a plan: a tier-derived profile value overwrites
//!      whatever is already there with the account's current subscription
//!      state, multiplier included. That a file-derived `subscriptionType`
//!      never itself carries a multiplier is an empirical observation about
//!      upstream data, not a structural guarantee this logic depends on. An
//!      `organization_type`-derived value, or no profile result at all,
//!      must never overwrite an existing value. This path only ever reads a token already on disk — it never
//!      refreshes and never writes back, so a `claude setup-token` login and
//!      an account whose on-disk token has already expired are simply left
//!      at whatever source 1 already resolved (a file plan, or none) until
//!      normal traffic elsewhere refreshes the file. The dashboard renders
//!      the pool once per page load, not on a periodic timer — the cache
//!      instead bounds repeat cost across an operator's page loads and any
//!      direct API client that hits this endpoint. The backfill attempt
//!      itself is time-boxed by [`BackfillBudgets`], so a stalled Claude
//!      endpoint can never make `GET /admin/pool` hang; that bound covers
//!      only this plan-resolution stage, not the account list itself, which
//!      comes from [`crate::auth::shared::resolve_pool_accounts`]'s
//!      unbounded credential-store scan.
//!   3. Otherwise, absent — the pool response simply omits the `plan` key
//!      for that account.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime},
};

use serde_json::Value;

use crate::{
    accounts::{account_key, AccountKey},
    auth::{
        claude::{auth as claude_auth, store as claude_store},
        codex::store as codex_store,
        shared::{claude_plan_from_credentials, codex_plan_from_credentials},
    },
    config::{AccountConfig, AuthMode},
};

/// Derive a plan label from a Claude `GET /api/oauth/profile` response, used
/// to backfill accounts whose credential file carries no `subscriptionType`
/// and to refine accounts whose file-derived plan carries no multiplier
/// information. Two source fields, most specific first:
///   - `organization.rate_limit_tier`, e.g. `"default_claude_max_20x"` ->
///     `"max 20x"`. Anthropic's own naming for the per-seat multiplier tiers
///     (`5x`, `20x`, …) sold above the base "max" plan.
///   - `organization.organization_type`, e.g. `"claude_max"` -> `"max"`, the
///     coarser field present even without a multiplier tier.
///
/// The returned `bool` is `true` only for a `rate_limit_tier`-derived value —
/// the one shape carrying multiplier information a caller may use to
/// overwrite a coarser existing plan (file-derived or `organization_type`-
/// derived). It is `false` for an `organization_type`-derived value, which
/// must only ever fill a genuinely empty slot; see
/// [`backfill_claude_profile_plans`]'s merge rule. Never infer this from the
/// returned string's shape — there is no reliable textual distinction
/// between the two, only the source field tells them apart.
///
/// An unrecognized shape (neither field present, or a `rate_limit_tier` that
/// doesn't match the `default_claude_{plan}_{N}x` pattern and no fallback
/// `organization_type`) yields `None` rather than guessing — this is a
/// display label, and a wrong guess is worse than a blank cell.
pub(crate) fn claude_plan_from_profile(value: &Value) -> Option<(String, bool)> {
    let organization = value.get("organization")?;
    if let Some(tier) = organization.get("rate_limit_tier").and_then(Value::as_str) {
        if let Some(plan) = parse_rate_limit_tier(tier) {
            return Some((plan, true));
        }
    }
    organization
        .get("organization_type")
        .and_then(Value::as_str)
        .and_then(parse_organization_type)
        .map(|plan| (plan, false))
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
    /// by one `/admin/pool` request — with one deliberate exception: since
    /// [`file_derived_plans`] floors its own timeout at `min_slice` above
    /// whatever `deadline` has left, the true wall-clock upper bound is
    /// `total + (n - 1) * min_slice`, where `n` is the number of
    /// `claude_oauth`/`chatgpt_oauth` providers in the pool (a `kimi_oauth`
    /// provider returns before ever touching the filesystem, so it never
    /// contributes a floor). This formula assumes `min_slice <= total`: the
    /// production `admin::pool` handler's `BackfillBudgets::default` (8s /
    /// 2s) satisfies it, and that handler never constructs any other budget
    /// (see its "only the production default is ever used on this path"
    /// comment). A shrunk test-only budget may not: if `min_slice > total`,
    /// the floor dominates and the true bound becomes a multiple of
    /// `min_slice` instead of `total`. Computed once, before the per-provider
    /// loop begins, and shared by every provider in that one request — a
    /// stalled account in an earlier provider must not let a later provider
    /// spend its own separate budget on top, except for that one floor, so
    /// that an earlier provider's budget exhaustion can never erase a later
    /// provider's free local credential-file read.
    pub(crate) total: Duration,
    /// Upper bound on one account's resolve-plus-fetch attempt.
    pub(crate) per_account: Duration,
    /// Two roles: (1) below this much remaining budget,
    /// [`backfill_claude_profile_plans`] skips an account's attempt rather
    /// than start one almost certain to be cut off mid-flight; (2)
    /// [`file_derived_plans`] floors its own timeout at this much above
    /// `deadline`, so the free local credential-file read it performs is
    /// always guaranteed at least this long even when an earlier provider's
    /// backfill has already exhausted the shared deadline.
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
    // The file phase carries its own `min_slice` floor above `deadline` (see
    // `file_derived_plans`), so it only returns `None` in the pathological
    // case where even that floor is not enough. Fall through to an empty
    // `FileDerivedPlans` rather than returning early: with no tokens
    // harvested, `backfill_claude_profile_plans` will find zero backfill
    // candidates and never touch the network, but its cache-harvest step
    // (which runs before any token check) still surfaces whatever plan is
    // already cached for these accounts.
    let file_derived = file_derived_plans(auth, accounts, budgets, deadline)
        .await
        .unwrap_or_default();
    let mut plans = file_derived.plans;
    if matches!(auth, AuthMode::ClaudeOauth) {
        backfill_claude_profile_plans(
            upstream,
            base_url,
            client,
            accounts,
            &file_derived.tokens,
            budgets,
            deadline,
            &mut plans,
        )
        .await;
    }
    plans
}

/// Path to an account's own credential file: an explicit `credentials` path
/// (already tilde-expanded at config parse — see `config::AccountConfig`),
/// otherwise the store path for its family. Mirrors the real resolvers
/// (`crate::auth::claude`/`crate::auth::codex`): a scanned store entry
/// always has `credentials: None`, so the store-path fallback already
/// covers it without a separate `store_entry` branch. A `token_env`-only
/// account has no file to read and is skipped.
fn credential_path(auth: AuthMode, account: &AccountConfig) -> Option<PathBuf> {
    if account.token_env.is_some() {
        return None;
    }
    if let Some(path) = account.credentials.as_deref() {
        return Some(PathBuf::from(path));
    }
    Some(match auth {
        AuthMode::ChatgptOauth => codex_store::account_path(&account.name),
        _ => claude_store::account_path(&account.name),
    })
}

/// Per-account results of one [`file_derived_plans`] pass: file-derived
/// plans, and, for every Claude account regardless of whether its file
/// yielded a plan, any still-valid on-disk token found via
/// [`crate::auth::claude::auth::refreshable_valid_access_token`] on that same
/// read — an account with a file-derived plan is still a refinement
/// candidate for the live profile lookup, and that lookup needs the token
/// from here to run at all.
#[derive(Default)]
struct FileDerivedPlans {
    plans: HashMap<String, String>,
    tokens: HashMap<String, String>,
}

/// Read every resolvable credential file and extract a plan from each, in one
/// batched `spawn_blocking` call (never one task per account — a pool can hold
/// dozens of accounts, and each is a synchronous file read that must not run
/// on a runtime worker thread). Kimi (and any other unsupported family) short
/// circuits before touching the filesystem at all.
///
/// For every Claude account, also extracts its still-valid on-disk access
/// token ([`crate::auth::claude::auth::refreshable_valid_access_token`]) from
/// that same read — the one signal [`backfill_claude_profile_plans`] needs to
/// decide candidacy, so it never has to re-open the file. This runs
/// regardless of whether the file already yielded a plan: a file-derived
/// plan carries no multiplier information, so it can still be a refinement
/// candidate for the live profile lookup, and that lookup needs the token
/// [`backfill_claude_profile_plans`] would otherwise have no way to obtain.
///
/// Returns `Some` for every normal path, including both early returns below
/// (an unsupported family, or no candidate files at all) — an empty result
/// is still a resolved result. Returns `None` only in the pathological case
/// where the batched file read does not finish even before `file_deadline`
/// (`deadline` raised by `budgets.min_slice`, see below): a truly blocked
/// `spawn_blocking` thread cannot be cancelled, so this timeout bounds how
/// long the caller waits on it, not the read itself.
async fn file_derived_plans(
    auth: AuthMode,
    accounts: &[AccountConfig],
    budgets: &BackfillBudgets,
    deadline: tokio::time::Instant,
) -> Option<FileDerivedPlans> {
    let extractor: fn(&Value) -> Option<String> = match auth {
        AuthMode::ClaudeOauth => claude_plan_from_credentials,
        AuthMode::ChatgptOauth => codex_plan_from_credentials,
        _ => return Some(FileDerivedPlans::default()),
    };
    let candidates: Vec<(String, PathBuf)> = accounts
        .iter()
        .filter_map(|account| {
            credential_path(auth, account).map(|path| (account.name.clone(), path))
        })
        .collect();
    if candidates.is_empty() {
        return Some(FileDerivedPlans::default());
    }
    let candidate_count = candidates.len();
    let now = SystemTime::now();
    let read = tokio::task::spawn_blocking(move || {
        let mut result = FileDerivedPlans::default();
        for (name, path) in candidates {
            let Some(value) = read_json(&path) else {
                continue;
            };
            if let Some(plan) = extractor(&value) {
                result.plans.insert(name.clone(), plan);
            }
            if matches!(auth, AuthMode::ClaudeOauth) {
                if let Some(token) = claude_auth::refreshable_valid_access_token(&value, now) {
                    result.tokens.insert(name, token);
                }
            }
        }
        result
    });
    // Guarantee the file phase its own `min_slice` floor above whatever the
    // shared `deadline` has left, so an earlier provider's budget
    // exhaustion can never cut off this provider's free local
    // credential-file read.
    let file_deadline = deadline.max(tokio::time::Instant::now() + budgets.min_slice);
    match tokio::time::timeout_at(file_deadline, read).await {
        Ok(Ok(result)) => Some(result),
        Ok(Err(join_error)) => {
            tracing::debug!(
                candidates = candidate_count,
                %join_error,
                "admin pool: file-phase credential read task panicked"
            );
            Some(FileDerivedPlans::default())
        }
        Err(_elapsed) => {
            tracing::debug!(
                candidates = candidate_count,
                "admin pool: file-phase credential read timed out even against the min_slice floor"
            );
            None
        }
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// A resolved (or attempted-and-failed) Claude profile plan lookup, cached for
/// the process lifetime. Success is cached far longer than failure: a plan
/// tier changes rarely (an operator upgrading their subscription), while a
/// failure (network blip, token not yet refreshable) is worth retrying soon.
/// For an account that also holds a file-derived plan, a cached failure
/// never blanks the display — [`backfill_claude_profile_plans`]'s merge step
/// falls back to the file value, exactly as an uncached failure would.
///
/// `from_tier` records whether `plan` (when present) came from
/// `organization.rate_limit_tier` (`true`) or the coarser
/// `organization.organization_type` (`false`) — see
/// [`claude_plan_from_profile`]. Never inferred from `plan`'s string shape;
/// always threaded through explicitly from the source lookup.
struct CachedProfilePlan {
    plan: Option<String>,
    from_tier: bool,
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
/// `account_key` resolves a configured account with a `uuid` as `Verified`,
/// a scanned store entry as `StoreEntry`, and a uuid-less name-only
/// configured account as `UpstreamInline`; that last key is the
/// `(upstream, name)` pair, which is stable for the lifetime of this
/// process-wide cache just like the other two variants.
fn profile_cache() -> &'static Mutex<HashMap<AccountKey, CachedProfilePlan>> {
    static CACHE: OnceLock<Mutex<HashMap<AccountKey, CachedProfilePlan>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_profile_plan(key: &AccountKey) -> Option<Option<(String, bool)>> {
    let cache = profile_cache().lock().expect("plan cache lock poisoned");
    cache
        .get(key)
        .filter(|entry| !entry.is_stale())
        .map(|entry| entry.plan.clone().map(|plan| (plan, entry.from_tier)))
}

fn store_profile_plan(key: AccountKey, resolved: Option<(String, bool)>) {
    let mut cache = profile_cache().lock().expect("plan cache lock poisoned");
    let (plan, from_tier) = match resolved {
        Some((plan, from_tier)) => (Some(plan), from_tier),
        None => (None, false),
    };
    cache.insert(
        key,
        CachedProfilePlan {
            plan,
            from_tier,
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

/// For every `claude_oauth` account that still holds a valid on-disk access
/// token ([`crate::auth::claude::auth::refreshable_valid_access_token`], read
/// by [`file_derived_plans`] into `tokens`), attempt a live profile fetch and
/// merge the result onto whatever plan the account already has — including
/// an account whose file already carries a `subscriptionType`, since a
/// `rate_limit_tier`-derived profile value overwrites whatever is already
/// there with the account's current subscription state, multiplier
/// included (that a file-derived `subscriptionType` never itself carries a
/// multiplier is an empirical observation about upstream data, not a
/// structural guarantee this logic depends on). A `claude setup-token` login has no refresh
/// token to satisfy `refreshable_valid_access_token` and is therefore never a
/// candidate, and neither is an account whose on-disk token has already
/// expired — this path never refreshes and never writes back, so such an
/// account simply stays at whatever [`file_derived_plans`] already resolved
/// (a file plan, or none) until normal traffic elsewhere refreshes its file.
/// Every failure (network error, non-2xx, unparseable body, or a timeout
/// against `budgets`/`deadline`) degrades to leaving the existing plan (file
/// value or none) untouched rather than propagating — this field is purely
/// informational, so it must never turn a working pool dashboard into a
/// broken one.
///
/// Merge rule, enforced by [`merge_profile_plan`] alone — every insertion
/// point below routes through it so the rule cannot drift between call
/// sites: a tier-derived profile value overwrites whatever is already there
/// with the account's current subscription state, multiplier included; an
/// `organization_type`-derived value only fills a genuinely empty slot,
/// never replacing an existing plan; no profile result at all leaves the
/// existing plan untouched. A file-derived plan can therefore never
/// disappear or be downgraded through this step, only ever be refined
/// toward a more precise value.
///
/// Flow: (1) a profile-cache hit is harvested and merged regardless of
/// remaining budget or token eligibility — reading the cache is free, and a
/// missing token today does not invalidate what an earlier, eligible attempt
/// already resolved; (2) every other account that still holds an eligible token
/// becomes an attempt candidate, ordered so accounts with no known plan at all
/// are attempted before accounts that already have a file-derived plan to
/// refine — within this one call, a plan-holding account is already showing
/// something useful, so it never consumes the budget a plan-less account needs
/// first. That ordering only holds within one call, i.e. one provider's account
/// list: `deadline` (see the caller in `src/admin/mod.rs`) is shared across the
/// entire provider loop of a single request, so a second `claude_oauth`
/// provider's plan-less accounts can still be starved by an earlier provider's
/// refinement candidates. Since a resolved plan is cached for 24h, that
/// starvation is one-time and self-corrects on a later request. With no
/// candidates, this returns immediately without ever touching [`BACKFILL_LOCK`]
/// — eligibility is decided entirely from `tokens`, already in memory from the
/// file phase, so this fast path needs no further file I/O or lock acquisition.
/// Otherwise the lock is acquired
/// with a `deadline`-bounded `timeout_at` — on timeout, only what step (1)
/// already harvested is kept. Unlike [`file_derived_plans`], this lock wait
/// and the per-attempt budget below deliberately keep the plain `deadline`
/// with no floor raised above it: this network-facing path is the real
/// defense against a stalled remote endpoint (see the module doc's `R3`
/// reference), so it must stay strictly bounded rather than run past budget
/// the way the free local file read now can. Once inside the lock, each candidate is
/// re-checked against the cache (another concurrent caller may have
/// resolved it while this caller waited — the same double-check
/// `crate::auth::claude::auth`'s `get_valid_access_token` performs on
/// `REFRESH_LOCK`, see `src/auth/claude/auth.rs:123-129`), then attempted
/// with `tokio::time::timeout(min(budgets.per_account, remaining), ..)`. A
/// candidate whose remaining budget has already dropped below
/// `budgets.min_slice` is skipped without an attempt and without a cache
/// write — a future request with a fresh budget should still get a real
/// attempt, not inherit a false "no plan" left by a starved one. This also
/// means a plan-refinement candidate skipped for lack of remaining budget
/// simply keeps showing its file value and is retried on a later page load
/// once budget is available, converging over repeated requests even when
/// one request has more eligible accounts than its budget covers.
#[allow(clippy::too_many_arguments)]
async fn backfill_claude_profile_plans(
    upstream: &str,
    base_url: &str,
    client: &reqwest::Client,
    accounts: &[AccountConfig],
    tokens: &HashMap<String, String>,
    budgets: &BackfillBudgets,
    deadline: tokio::time::Instant,
    plans: &mut HashMap<String, String>,
) {
    // Two tiers, attempted in order: an account with no known plan at all
    // goes first, so a refinement candidate (one that already has a
    // file-derived plan and is only asking for a more precise value) never
    // consumes the budget a plan-less account needs first — within this one
    // call. `deadline` is shared across every provider in the outer loop
    // (`src/admin/mod.rs`), so this ordering gives no such guarantee across
    // multiple `claude_oauth` providers in the same request; see the doc
    // comment above for why that starvation is one-time and self-corrects.
    let mut new_candidates: Vec<(&AccountConfig, AccountKey)> = Vec::new();
    let mut refinement_candidates: Vec<(&AccountConfig, AccountKey)> = Vec::new();
    for account in accounts {
        let key = account_key(upstream, account);
        if let Some(cached) = cached_profile_plan(&key) {
            merge_profile_plan(plans, &account.name, cached);
            continue;
        }
        if tokens.contains_key(&account.name) {
            if plans.contains_key(&account.name) {
                refinement_candidates.push((account, key));
            } else {
                new_candidates.push((account, key));
            }
        }
    }
    let candidates: Vec<(&AccountConfig, AccountKey)> = new_candidates
        .into_iter()
        .chain(refinement_candidates)
        .collect();
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
            merge_profile_plan(plans, &account.name, cached);
            continue;
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining < budgets.min_slice {
            continue;
        }

        // `tokens` guaranteed this account an entry when it became a
        // candidate above; nothing between then and now removes it.
        let Some(access_token) = tokens.get(&account.name) else {
            continue;
        };

        let attempt_budget = budgets.per_account.min(remaining);
        match tokio::time::timeout(
            attempt_budget,
            fetch_claude_profile_plan(base_url, client, access_token),
        )
        .await
        {
            Ok(Some(resolved)) => {
                store_profile_plan(key, Some(resolved.clone()));
                merge_profile_plan(plans, &account.name, Some(resolved));
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

/// Merge a resolved profile plan lookup onto an account's existing plan
/// entry (a file-derived value, an earlier merge's result, or nothing) — the
/// ONE place this rule is expressed; every insertion point in
/// [`backfill_claude_profile_plans`] (cache-hit pre-lock, cache-hit
/// post-lock double-check, and fresh-fetch success) routes through this
/// function so the rule cannot drift between call sites. A tier-derived
/// value (`from_tier: true`) always overwrites whatever is already there
/// with the account's current subscription state, multiplier included —
/// that a file-derived `subscriptionType` never itself carries a multiplier
/// is an empirical observation about upstream data, not a structural
/// guarantee this logic depends on. A coarser value (`from_tier: false`,
/// `organization_type`-derived) only fills a slot that is genuinely empty —
/// it must never replace an existing plan, file-derived or otherwise, since
/// that would silently discard information. `None` (no profile result at
/// all: cache miss with no entry, a failed fetch, or a timeout) never
/// touches the existing entry.
fn merge_profile_plan(
    plans: &mut HashMap<String, String>,
    name: &str,
    profile: Option<(String, bool)>,
) {
    let Some((plan, from_tier)) = profile else {
        return;
    };
    if from_tier || !plans.contains_key(name) {
        plans.insert(name.to_string(), plan);
    }
}

/// One `GET {base_url}/api/oauth/profile` call against a token already known
/// valid on disk (`tokens`, from [`file_derived_plans`]), mirroring the
/// OAuth bearer + header pattern `claude::usage::fetch_usage` already uses
/// against the adjacent `/api/oauth/usage` endpoint. This function never
/// resolves or refreshes a credential itself — the admin path must never
/// touch `REFRESH_LOCK` or write a rotated token back to disk — so the
/// per-attempt `tokio::time::timeout` the caller wraps this in bounds only
/// the HTTP call, with the `reqwest` timeout below kept as a secondary
/// defense-in-depth layer.
async fn fetch_claude_profile_plan(
    base_url: &str,
    client: &reqwest::Client,
    access_token: &str,
) -> Option<(String, bool)> {
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
        assert_eq!(
            claude_plan_from_profile(&value),
            Some(("max 20x".to_string(), true))
        );
    }

    #[test]
    fn claude_plan_from_profile_falls_back_to_organization_type() {
        let no_tier = serde_json::json!({"organization": {"organization_type": "claude_max"}});
        assert_eq!(
            claude_plan_from_profile(&no_tier),
            Some(("max".to_string(), false))
        );

        let unmatched_tier = serde_json::json!({"organization": {
            "rate_limit_tier": "some_other_shape",
            "organization_type": "claude_pro"
        }});
        assert_eq!(
            claude_plan_from_profile(&unmatched_tier),
            Some(("pro".to_string(), false))
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

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let result = file_derived_plans(
            AuthMode::ClaudeOauth,
            &accounts,
            &BackfillBudgets::default(),
            deadline,
        )
        .await
        .expect("file phase must not time out against a 5s deadline");
        assert_eq!(
            result.plans.get("with-plan").map(String::as_str),
            Some("max")
        );
        assert_eq!(result.plans.get("env-only"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn file_derived_plans_empty_for_unsupported_family() {
        let accounts = vec![AccountConfig {
            name: "kimi-account".to_string(),
            credentials: Some("/does/not/matter".to_string()),
            ..Default::default()
        }];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let result = file_derived_plans(
            AuthMode::KimiOauth,
            &accounts,
            &BackfillBudgets::default(),
            deadline,
        )
        .await
        .expect("unsupported family short-circuits before any timeout is possible");
        assert!(result.plans.is_empty());
        assert!(result.tokens.is_empty());
    }

    /// A refreshable-login fixture for the backfill tests below: a non-empty
    /// `refreshToken` and an `expiresAt` far in the future — the exact shape
    /// [`crate::auth::claude::auth::refreshable_valid_access_token`] treats
    /// as a still-valid on-disk access token, so `file_derived_plans` puts
    /// this account's token straight into its `tokens` map, no network round
    /// trip needed to establish eligibility. `subscription_type` is `None`
    /// for the plain no-file-plan tests (so `file_derived_plans` yields no
    /// plan and the profile backfill path is the only source) and `Some` for
    /// the refinement tests below, which need a file-derived plan already
    /// present alongside a valid token.
    fn write_refreshable_fixture(
        dir: &std::path::Path,
        name: &str,
        subscription_type: Option<&str>,
    ) -> PathBuf {
        let expires_at_ms = (std::time::SystemTime::now() + Duration::from_secs(3600))
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let path = dir.join(format!("{name}.json"));
        let mut oauth = serde_json::json!({
            "accessToken": "access-token",
            "refreshToken": "refresh-token",
            "expiresAt": expires_at_ms
        });
        if let Some(plan) = subscription_type {
            oauth["subscriptionType"] = serde_json::Value::String(plan.to_string());
        }
        std::fs::write(
            &path,
            serde_json::json!({"claudeAiOauth": oauth}).to_string(),
        )
        .unwrap();
        path
    }

    /// [`write_refreshable_fixture`] plus a top-level `padding` sibling of
    /// `padding_bytes` filler characters, appended by reading the file back
    /// and rewriting it -- see the comment where this is used for why some
    /// fixtures need to be this large.
    fn write_padded_refreshable_fixture(
        dir: &std::path::Path,
        name: &str,
        subscription_type: Option<&str>,
        padding_bytes: usize,
    ) -> PathBuf {
        let path = write_refreshable_fixture(dir, name, subscription_type);
        let mut value: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value["padding"] = Value::String("x".repeat(padding_bytes));
        std::fs::write(&path, value.to_string()).unwrap();
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
        let creds_path = write_refreshable_fixture(&dir, "stalled-account", None);
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
        let creds_path = write_refreshable_fixture(&dir, "starved-account", None);
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
        let creds_path = write_refreshable_fixture(&dir, "concurrent-account", None);
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

    #[tokio::test]
    async fn backfill_refines_a_file_plan_when_profile_reports_a_tier_multiplier() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        reset_profile_cache();
        let dir = unique_test_dir("refine-tier");
        std::fs::create_dir_all(&dir).unwrap();
        let creds_path = write_refreshable_fixture(&dir, "refine-tier-account", Some("max"));
        let account = AccountConfig {
            name: "refine-tier-account".to_string(),
            uuid: Some("refine-tier-account-uuid".to_string()),
            credentials: Some(creds_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/profile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "organization": {"rate_limit_tier": "default_claude_max_20x"}
            })))
            .mount(&server)
            .await;

        let budgets = BackfillBudgets::default();
        let deadline = tokio::time::Instant::now() + budgets.total;
        let client = reqwest::Client::new();
        let plans = plans_for_accounts(
            AuthMode::ClaudeOauth,
            "refine-tier-test-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &budgets,
            deadline,
        )
        .await;

        assert_eq!(
            plans.get("refine-tier-account").map(String::as_str),
            Some("max 20x"),
            "a tier-derived profile value must refine a coarser file-derived plan"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "an existing file plan must not block a refinement attempt when a valid token is present"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn backfill_never_downgrades_a_file_plan_with_an_organization_type_value() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        reset_profile_cache();
        let dir = unique_test_dir("refine-no-downgrade");
        std::fs::create_dir_all(&dir).unwrap();
        let creds_path = write_refreshable_fixture(&dir, "refine-guard-account", Some("max"));
        let account = AccountConfig {
            name: "refine-guard-account".to_string(),
            uuid: Some("refine-guard-account-uuid".to_string()),
            credentials: Some(creds_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/profile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "organization": {"organization_type": "claude_pro"}
            })))
            .mount(&server)
            .await;

        let budgets = BackfillBudgets::default();
        let deadline = tokio::time::Instant::now() + budgets.total;
        let client = reqwest::Client::new();
        let plans = plans_for_accounts(
            AuthMode::ClaudeOauth,
            "refine-guard-test-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &budgets,
            deadline,
        )
        .await;

        assert_eq!(
            plans.get("refine-guard-account").map(String::as_str),
            Some("max"),
            "an organization_type-derived profile value must never overwrite an existing file-derived plan"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "the account must actually have been attempted, not merely left alone for an unrelated reason"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn file_derived_plans_floors_its_own_deadline_at_min_slice() {
        reset_profile_cache();
        let dir = unique_test_dir("floor");
        std::fs::create_dir_all(&dir).unwrap();
        let creds_path = dir.join("floor-account.json");
        // Oversized on purpose: without this, a tiny fixture can finish
        // reading well inside the ~1ms grace `timeout_at` grants an
        // already-expired deadline, making the min_slice-floor guarantee
        // below vacuous (see the long comment further down in this module
        // for the full mechanism).
        std::fs::write(
            &creds_path,
            serde_json::json!({
                "claudeAiOauth": {"subscriptionType": "max"},
                "padding": "x".repeat(2 * 1024 * 1024)
            })
            .to_string(),
        )
        .unwrap();
        let account = AccountConfig {
            name: "floor-account".to_string(),
            credentials: Some(creds_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let budgets = BackfillBudgets::default();
        // Already expired: without the min_slice floor, timeout_at sees an
        // instantly-elapsed deadline and file_derived_plans incorrectly
        // returns None -- the file read itself was microseconds-scale before
        // the `padding` key above was introduced, and padding now pushes
        // read+parse to millisecond scale so it reliably clears the timer's
        // round-up grace instead of slipping under it. This mitigation is
        // probabilistic rather than an absolute guarantee: padding widens the
        // race window by orders of magnitude without proving it closed, and
        // the escape-rate upper bound this relies on is tracked by
        // mutation-repetition measurement, not by this padding size alone --
        // re-run that measurement before shrinking it.
        let deadline = tokio::time::Instant::now() - Duration::from_secs(1);

        let result = file_derived_plans(
            AuthMode::ClaudeOauth,
            std::slice::from_ref(&account),
            &budgets,
            deadline,
        )
        .await;

        let result = result.expect(
            "the min_slice floor must give the file phase its own budget even past an \
             already-expired deadline",
        );
        assert_eq!(
            result.plans.get("floor-account").map(String::as_str),
            Some("max")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn plans_for_accounts_shares_one_deadline_across_multiple_providers() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        reset_profile_cache();

        // One deadline, computed once (below, right after the mock server is
        // mounted and right before the first call) and reused for both calls
        // -- reproducing how the `/admin/pool` handler in `src/admin/mod.rs`
        // shares a single deadline across its per-provider loop.
        let dir = unique_test_dir("mmp-first");
        std::fs::create_dir_all(&dir).unwrap();
        let creds_path = write_refreshable_fixture(&dir, "mmp-first-account", None);
        let first_account = AccountConfig {
            name: "mmp-first-account".to_string(),
            uuid: Some("mmp-first-account-uuid".to_string()),
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
            total: Duration::from_secs(2),
            // Must be at least `total`: `backfill_claude_profile_plans` bounds
            // one attempt at `budgets.per_account.min(remaining)`, so a
            // smaller `per_account` would cap the stalled first attempt short
            // of the shared deadline, leaving it unexhausted and making the
            // self-check assertion below vacuous even without any mutation.
            per_account: Duration::from_secs(2),
            // `min_slice` (and so `total - min_slice`, the skip band's width
            // the first provider must clear without tripping
            // `remaining < budgets.min_slice` in
            // `backfill_claude_profile_plans`) needs headroom above real
            // scheduling delay: a full `cargo test --lib` run, with ~1700
            // other tests contending for this 4-core box, was observed to
            // push the gap between computing `deadline` and this test's
            // first attempt actually running well past what a tight budget
            // can absorb. 400ms stays far above that observed gap while
            // remaining far below `total`, and still leaves comfortable
            // headroom above what the second provider's local
            // credential-file read needs -- before the `padding` key on
            // that fixture was introduced the read was microseconds-scale;
            // padding now puts read+parse at millisecond scale, still well
            // under this floor. This mitigation is probabilistic rather than
            // an absolute guarantee: padding widens the race window by
            // orders of magnitude without proving it closed, and the
            // escape-rate upper bound this relies on is tracked by
            // mutation-repetition measurement, not by this padding size
            // alone -- re-run that measurement before shrinking it.
            min_slice: Duration::from_millis(400),
        };
        // Computed here, after the mock server is up and mounted and
        // immediately before the first call below -- not earlier, or the
        // async setup above would eat into the window this test relies on to
        // exhaust the deadline via one genuinely stalled attempt. Mirrors the
        // mock-then-deadline order in
        // `backfill_bounds_total_time_when_profile_endpoint_stalls`.
        let deadline = tokio::time::Instant::now() + budgets.total;
        let client = reqwest::Client::new();

        let _first_plans = plans_for_accounts(
            AuthMode::ClaudeOauth,
            "mmp-first-provider-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&first_account),
            &budgets,
            deadline,
        )
        .await;
        // `per_account >= total` above forces the stalled attempt to run
        // until `deadline` itself rather than a smaller per-account cap, so
        // this holds on every path that actually attempts or waits on the
        // lock -- it only fails on the skip path, which is exactly the path
        // that would make the min_slice-floor guarantee below vacuous.
        assert!(
            tokio::time::Instant::now() >= deadline,
            "the first provider must consume the whole shared deadline, or this test's floor guarantee is vacuous"
        );

        // Second provider, a different upstream name and account: the file
        // carries a plan but no refresh token at all, so it has zero
        // backfill candidates and can never touch the network regardless of
        // how much of the shared deadline the first provider consumed.
        let second_dir = unique_test_dir("mmp-second");
        std::fs::create_dir_all(&second_dir).unwrap();
        let second_creds_path = second_dir.join("mmp-second-account.json");
        // Oversized on purpose: without this, a tiny fixture can finish
        // reading well inside the ~1ms grace `timeout_at` grants an
        // already-expired deadline, making the min_slice-floor guarantee
        // below vacuous (see the long comment further down in this module
        // for the full mechanism).
        std::fs::write(
            &second_creds_path,
            serde_json::json!({
                "claudeAiOauth": {"subscriptionType": "max"},
                "padding": "x".repeat(2 * 1024 * 1024)
            })
            .to_string(),
        )
        .unwrap();
        let second_account = AccountConfig {
            name: "mmp-second-account".to_string(),
            uuid: Some("mmp-second-account-uuid".to_string()),
            credentials: Some(second_creds_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let second_plans = plans_for_accounts(
            AuthMode::ClaudeOauth,
            "mmp-second-provider-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&second_account),
            &budgets,
            deadline,
        )
        .await;

        assert_eq!(
            second_plans.get("mmp-second-account").map(String::as_str),
            Some("max"),
            "a second provider with zero network candidates must still surface its \
             file-derived plan even after an earlier provider consumed part of the \
             shared deadline"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&second_dir);
    }

    #[tokio::test]
    async fn plans_for_accounts_harvests_cache_even_when_the_file_phase_times_out() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        reset_profile_cache();
        let dir = unique_test_dir("cache-harvest");
        std::fs::create_dir_all(&dir).unwrap();
        // Oversized on purpose. `tokio::time::timeout_at` polls its inner future
        // before the delay, and tokio rounds a timer deadline up to the next
        // millisecond tick -- so an already-expired deadline still grants the
        // `spawn_blocking` read up to ~1ms of slack, and a few-hundred-byte
        // credential file finishes well inside it. That would hand back `Ok` and
        // silently skip the timeout branch this test exists to cover. A
        // multi-megabyte `padding` sibling puts read+parse into the milliseconds,
        // past that window by an order of magnitude, while staying far under the
        // `min_slice` floor the normal path relies on. The key sits beside
        // `claudeAiOauth`, which both extractors navigate into by name
        // (`auth::shared::claude_plan_from_credentials`,
        // `auth::claude::auth::refreshable_valid_access_token`), so it changes no
        // extracted value. This mitigation is probabilistic rather than an
        // absolute guarantee: padding widens the race window by orders of
        // magnitude without proving it closed, and the escape-rate upper
        // bound this relies on is tracked by mutation-repetition
        // measurement, not by this padding size alone -- re-run that
        // measurement before shrinking it. Do not "fix" a flake here with
        // `start_paused`: virtual time auto-advances while a
        // `spawn_blocking` is in flight and would fire the timeout on the
        // normal path too.
        let creds_path =
            write_padded_refreshable_fixture(&dir, "cache-harvest-account", None, 2 * 1024 * 1024);
        let account = AccountConfig {
            name: "cache-harvest-account".to_string(),
            uuid: Some("cache-harvest-account-uuid".to_string()),
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

        // First call: normal budgets and a future deadline resolve the plan
        // for real and cache it, keyed by AccountKey::Verified since the
        // account carries a uuid.
        let first_budgets = BackfillBudgets::default();
        let first_deadline = tokio::time::Instant::now() + first_budgets.total;
        let first_plans = plans_for_accounts(
            AuthMode::ClaudeOauth,
            "cache-harvest-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &first_budgets,
            first_deadline,
        )
        .await;
        assert_eq!(
            first_plans.get("cache-harvest-account").map(String::as_str),
            Some("max")
        );

        // Second call: same upstream and same uuid, so the same AccountKey
        // hits the warm cache -- but min_slice is zeroed out (defeating the
        // floor entirely) and the deadline is already expired, forcing the
        // file phase's timeout_at to fire and unwrap_or_default() to an
        // empty FileDerivedPlans. The cache-hit check inside
        // backfill_claude_profile_plans runs before any token-eligibility
        // check, so the plan must still surface purely from cache, with no
        // additional network request.
        let second_budgets = BackfillBudgets {
            total: Duration::from_secs(8),
            per_account: Duration::from_secs(5),
            min_slice: Duration::ZERO,
        };
        let second_deadline = tokio::time::Instant::now() - Duration::from_secs(1);
        let second_plans = plans_for_accounts(
            AuthMode::ClaudeOauth,
            "cache-harvest-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &second_budgets,
            second_deadline,
        )
        .await;

        assert_eq!(
            second_plans
                .get("cache-harvest-account")
                .map(String::as_str),
            Some("max"),
            "a cached plan must surface even when the file phase times out and min_slice is zeroed"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "the second call must be a pure cache harvest with zero additional network requests"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
