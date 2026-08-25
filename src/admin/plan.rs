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
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime},
};

use serde_json::Value;

use crate::{
    accounts::{account_key, AccountKey, AccountStateIdentity},
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
) -> Vec<Option<String>> {
    plans_for_accounts_under(
        file_read_lock(),
        auth,
        upstream,
        base_url,
        client,
        accounts,
        budgets,
        deadline,
    )
    .await
}

/// [`plans_for_accounts`] against a caller-supplied single-flight lock, for
/// the same reason as [`file_derived_plans`] — and additionally so a
/// test can make the file phase time out *deterministically*.
///
/// [`acquire_read_permit`] is `timeout_at(file_deadline, lock.lock_owned())`,
/// so a test that holds its own lock across the call leaves `lock_owned()`
/// pending on the first poll and an already-expired deadline then fires with
/// certainty. Inducing the same timeout by racing an oversized credential
/// file against that deadline does not: `timeout_at` polls its inner future
/// once and tokio rounds the timer up to the next millisecond tick, so
/// whether the read finishes inside that window depends on the machine. Two
/// tests here previously took that route and failed in CI while passing
/// locally.
#[allow(clippy::too_many_arguments)]
async fn plans_for_accounts_under(
    lock: &Arc<tokio::sync::Mutex<()>>,
    auth: AuthMode,
    upstream: &str,
    base_url: &str,
    client: &reqwest::Client,
    accounts: &[AccountConfig],
    budgets: &BackfillBudgets,
    deadline: tokio::time::Instant,
) -> Vec<Option<String>> {
    // The file phase carries its own `min_slice` floor above `deadline` (see
    // `file_derived_plans`), so it only returns `None` in the pathological
    // case where even that floor is not enough. Fall through to an all-absent
    // `FileDerivedPlans` rather than returning early: with no tokens
    // harvested, `backfill_claude_profile_plans` will find zero backfill
    // candidates and never touch the network, but its cache-harvest step
    // (which runs before any token check) still surfaces whatever plan is
    // already cached for these accounts.
    let read = file_derived_plans(lock, auth, accounts, budgets, deadline).await;
    // Distinct from "read fine, no uuid in the file": only a phase that never
    // produced a result may fall back to the remembered identity.
    let file_phase_timed_out = read.is_none();
    let file_derived = read.unwrap_or_else(|| FileDerivedPlans::empty(accounts.len()));
    let mut plans = file_derived.plans;
    if matches!(auth, AuthMode::ClaudeOauth) {
        backfill_claude_profile_plans(
            upstream,
            base_url,
            client,
            accounts,
            &file_derived.tokens,
            &file_derived.uuids,
            &file_derived.read_failed,
            file_phase_timed_out,
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
///
/// Every field is **positionally aligned** to the `accounts` slice handed to
/// [`file_derived_plans`]: `plans[i]`, `tokens[i]`, and `uuids[i]` all
/// describe `accounts[i]`. Never key any of these by `account.name`.
/// `resolve_pool_accounts` appends the configured accounts after the scoped
/// store entries without deduplicating names (`auth::shared`), so one
/// provider's resolved list may legitimately hold two distinct accounts
/// sharing a display name — a name-keyed map silently collapses them, and
/// for `tokens` that means probing one account with the other's bearer
/// token. The index is the only identifier guaranteed unique here.
struct FileDerivedPlans {
    plans: Vec<Option<String>>,
    tokens: Vec<Option<String>>,
    /// Per account, whether this pass tried to read its credential file and
    /// could not — missing, unreadable, or unparsable. Distinct from a uuid
    /// of `None`, which a perfectly healthy hand-placed credential also
    /// produces: only an actual read failure is evidence *against* the
    /// identity sitting on `AccountConfig::uuid`. See [`plan_cache_key`].
    read_failed: Vec<bool>,
    /// `shuntAccountUuid` as written by shunt's own credential import,
    /// harvested from the same read that produced `plans`/`tokens` so it
    /// costs no extra I/O. Used only to strengthen the profile cache key —
    /// see [`plan_cache_key`].
    uuids: Vec<Option<String>>,
}

impl FileDerivedPlans {
    /// All-absent results for `len` accounts. The vectors must always be
    /// exactly `accounts.len()` long, so this replaces a `Default` impl:
    /// an empty `Vec` would make every positional lookup an out-of-bounds
    /// read rather than a benign "no plan".
    fn empty(len: usize) -> Self {
        Self {
            plans: vec![None; len],
            tokens: vec![None; len],
            uuids: vec![None; len],
            read_failed: vec![false; len],
        }
    }
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
/// is still a resolved result. Returns `None` only when the batched file read
/// does not finish before `file_deadline` (`deadline` raised by
/// `budgets.min_slice`, see below), or when the single-flight permit below
/// could not be acquired within it.
///
/// A truly blocked `spawn_blocking` thread cannot be cancelled, so that
/// timeout bounds how long the caller waits on the read, not the read
/// itself. To keep a permanently stalled credential file (a hung FUSE or
/// network mount) from accumulating one leaked blocking worker per
/// `/admin/pool` request, the read runs under [`FILE_READ_LOCK`], and the
/// permit is **moved into the blocking closure** rather than held by this
/// future: it is released when the read actually finishes, not when this
/// function stops waiting for it. A stalled read therefore holds the permit
/// forever, and every later request fails to acquire it and returns `None`
/// without spawning anything — bounding the leak at one thread process-wide
/// instead of one per request.
///
/// `lock` is the single-flight lock to read under. Production always passes
/// [`file_read_lock`] (via [`plans_for_accounts`]); the parameter exists so a
/// test can supply its own. Two things need that: asserting the permit is
/// handed back after a completed read is order-dependent against the
/// process-wide lock, since a concurrently running test may legitimately be
/// holding it; and holding a private lock is how a test makes this function
/// return `None` deterministically (see [`plans_for_accounts_under`]).
async fn file_derived_plans(
    lock: &Arc<tokio::sync::Mutex<()>>,
    auth: AuthMode,
    accounts: &[AccountConfig],
    budgets: &BackfillBudgets,
    deadline: tokio::time::Instant,
) -> Option<FileDerivedPlans> {
    let extractor: fn(&Value) -> Option<String> = match auth {
        AuthMode::ClaudeOauth => claude_plan_from_credentials,
        AuthMode::ChatgptOauth => codex_plan_from_credentials,
        _ => return Some(FileDerivedPlans::empty(accounts.len())),
    };
    // `(index into accounts, path)` — never `(name, path)`; see
    // `FileDerivedPlans` for why the name is not a usable key here.
    let candidates: Vec<(usize, PathBuf)> = accounts
        .iter()
        .enumerate()
        .filter_map(|(index, account)| credential_path(auth, account).map(|path| (index, path)))
        .collect();
    if candidates.is_empty() {
        return Some(FileDerivedPlans::empty(accounts.len()));
    }
    let candidate_count = candidates.len();
    let now = SystemTime::now();
    let account_count = accounts.len();
    // Guarantee the file phase its own `min_slice` floor above whatever the
    // shared `deadline` has left, so an earlier provider's budget
    // exhaustion can never cut off this provider's free local
    // credential-file read.
    let file_deadline = deadline.max(tokio::time::Instant::now() + budgets.min_slice);
    let Some(permit) = acquire_read_permit(lock, file_deadline).await else {
        tracing::debug!(
            candidates = candidate_count,
            "admin pool: file-phase read permit wait timed out; an earlier credential read is \
             still blocked, so this request reads no credential files"
        );
        return None;
    };
    let read = tokio::task::spawn_blocking(move || {
        // Released only when this blocking read genuinely completes.
        let _permit = permit;
        let mut result = FileDerivedPlans::empty(account_count);
        for (index, path) in candidates {
            let Some(value) = read_json(&path) else {
                // Missing, unreadable, or unparsable. Whatever identity this
                // path once held is no longer evidenced by it, so drop the
                // memo here too -- not only on the parses-but-carries-no-uuid
                // branch below. Otherwise a later request whose file phase
                // times out takes the recall path and keys the profile cache
                // with that stale uuid. The bias is deliberate: clearing
                // costs at most a fallback to the name key and its 10-minute
                // TTL, while keeping it risks serving the previous occupant's
                // plan for the 24-hour exact-identity TTL. Ungated by family
                // so the clear is never narrower than the record above.
                forget_credential_uuid(&path);
                result.read_failed[index] = true;
                continue;
            };
            result.plans[index] = extractor(&value);
            if matches!(auth, AuthMode::ClaudeOauth) {
                result.tokens[index] = claude_auth::refreshable_valid_access_token(&value, now);
                let uuid = credential_account_uuid(&value);
                match uuid.as_deref() {
                    Some(uuid) => remember_credential_uuid(&path, uuid),
                    // The file was read and parsed; it simply carries no
                    // identity. Drop any uuid remembered for this path so a
                    // later timeout cannot resurrect the identity of an
                    // account this file no longer holds.
                    None => forget_credential_uuid(&path),
                }
                result.uuids[index] = uuid;
            }
        }
        result
    });
    match tokio::time::timeout_at(file_deadline, read).await {
        Ok(Ok(result)) => Some(result),
        Ok(Err(join_error)) => {
            tracing::debug!(
                candidates = candidate_count,
                %join_error,
                "admin pool: file-phase credential read task panicked"
            );
            Some(FileDerivedPlans::empty(account_count))
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

/// Process-wide single-flight for the batched credential-file read, held as
/// an owned permit across the blocking task's whole lifetime (see
/// [`file_derived_plans`]). An `Arc` rather than a plain `static` because
/// `lock_owned` is what lets the permit outlive the awaiting future.
fn file_read_lock() -> &'static Arc<tokio::sync::Mutex<()>> {
    static LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
}

/// Last `shuntAccountUuid` seen at a given credential path.
///
/// Keyed by **path**, deliberately: the path is what was actually read, and
/// two accounts that a display name cannot tell apart still have distinct
/// credential paths (or, when both fall back to the same store path, they are
/// reading the same file and so share the same identity anyway). A name key
/// would be ambiguous exactly where this matters.
///
/// Consulted only when a read could not supply the uuid itself — that is,
/// when the file phase timed out. Without it, a stalled read would key a
/// uuid-less account by name and miss the entry cached under its `Verified`
/// key, silently dropping the documented guarantee that an already-cached
/// plan still appears in the response.
fn credential_uuid_memo() -> &'static Mutex<HashMap<PathBuf, String>> {
    static MEMO: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember_credential_uuid(path: &Path, uuid: &str) {
    credential_uuid_memo()
        .lock()
        .expect("credential uuid memo lock poisoned")
        .insert(path.to_path_buf(), uuid.to_string());
}

fn forget_credential_uuid(path: &Path) {
    credential_uuid_memo()
        .lock()
        .expect("credential uuid memo lock poisoned")
        .remove(path);
}

fn recall_credential_uuid(path: &Path) -> Option<String> {
    credential_uuid_memo()
        .lock()
        .expect("credential uuid memo lock poisoned")
        .get(path)
        .cloned()
}

/// Wait for the single-flight permit, giving up at `file_deadline` rather
/// than queueing behind a read that may never finish. `None` means the
/// caller must not spawn: some earlier read still holds the permit, and
/// spawning anyway is exactly the unbounded accumulation this guards.
///
/// Takes the lock as a parameter so a test can exercise the give-up path
/// against its own lock — holding the process-wide one would make every
/// concurrently running test's credential read fail.
async fn acquire_read_permit(
    lock: &Arc<tokio::sync::Mutex<()>>,
    file_deadline: tokio::time::Instant,
) -> Option<tokio::sync::OwnedMutexGuard<()>> {
    tokio::time::timeout_at(file_deadline, lock.clone().lock_owned())
        .await
        .ok()
}

/// `shuntAccountUuid` from an already-parsed credential blob — the identity
/// shunt's own import stamps into the file. Mirrors
/// `auth::claude::store`'s reader, including its blank-uuid handling: a
/// whitespace-only value is a missing identity, never a distinct one that
/// could collide with another blank.
fn credential_account_uuid(value: &Value) -> Option<String> {
    value
        .get("shuntAccountUuid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|uuid| !uuid.is_empty())
        .map(ToOwned::to_owned)
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
    /// Whether this entry's key identifies the credential itself (a
    /// `Verified` uuid) rather than only a display name. Drives [`ttl`]:
    /// see there for why a name-keyed success must not be trusted for a day.
    ///
    /// [`ttl`]: CachedProfilePlan::ttl
    exact_identity: bool,
}

impl CachedProfilePlan {
    /// Success is cached far longer than failure — but only when the key
    /// actually identifies the credential.
    ///
    /// A `Verified` key carries the account's `shuntAccountUuid`, which
    /// survives token refresh and changes when the account is re-imported,
    /// so a resolved plan stays valid for a day. A `StoreEntry` /
    /// `UpstreamInline` key is only a name: remove a uuid-less account and
    /// re-provision a *different* subscription under the same store name and
    /// the key is byte-identical, so a day-long entry would keep showing the
    /// previous account's tier. Nothing in production clears this cache
    /// (`reset_profile_cache` is test-only), so the TTL is the sole bound on
    /// that staleness — hence the short one whenever identity is name-only.
    fn ttl(&self) -> Duration {
        match (self.plan.is_some(), self.exact_identity) {
            (true, true) => Duration::from_secs(24 * 60 * 60),
            (true, false) => Duration::from_secs(10 * 60),
            (false, _) => Duration::from_secs(10 * 60),
        }
    }

    fn is_stale(&self) -> bool {
        self.fetched_at.elapsed() >= self.ttl()
    }
}

/// The profile cache key for one account, plus whether it identifies the
/// credential exactly.
///
/// `account_key` alone resolves a configured account carrying a `uuid` as
/// `Verified`, a scanned store entry as `StoreEntry`, and a uuid-less
/// name-only configured account as `UpstreamInline`. The latter two are
/// names, not identities.
///
/// The credential file's own `shuntAccountUuid` — harvested for free by
/// [`file_derived_plans`] — outranks all three, because the value being
/// cached is the plan of whoever *that file's token* authenticates as.
/// `account.uuid` never selects the file ([`credential_path`] reads only
/// `credentials`/`name`), so it is a label attached to the account, not a
/// reading of the credential: an operator's stale `uuid = ` entry, or the
/// process-lifetime inline-identity memo in `auth::shared`, can both still
/// name the *previous* occupant of a re-provisioned path while the file
/// itself already names the new one. Keying off the label there would file
/// the new account's plan under the old account's identity and serve the old
/// account's plan back for a full day. So a present `credential_uuid` always
/// wins; `account.uuid` is the fallback for an account whose file could not
/// be read or carries no uuid.
///
/// `file_contradicts_account_uuid` is the same authority applied in reverse:
/// when this pass tried to read the account's credential file and could not,
/// that is evidence against whatever `account.uuid` still says, so the value
/// is stripped back to the name-based identity and the stale entry is simply
/// not found. Only a read *failure* counts — a file that parses and carries
/// no `shuntAccountUuid` is a healthy hand-placed credential whose configured
/// `uuid` is the legitimate identity, and a timed-out phase read nothing at
/// all. See the caller for how the three are told apart.
///
/// Only when no uuid exists anywhere (a hand-placed credential file shunt
/// never imported) does the name-based key survive, and
/// [`CachedProfilePlan::ttl`] then bounds how long a hit off it is trusted.
fn plan_cache_key(
    upstream: &str,
    account: &AccountConfig,
    credential_uuid: Option<&str>,
    file_contradicts_account_uuid: bool,
) -> (AccountKey, bool) {
    let key = account_key(upstream, account);
    if let Some(uuid) = credential_uuid {
        return (
            AccountKey {
                store_family: key.store_family,
                identity: AccountStateIdentity::Verified {
                    id: uuid.to_string(),
                },
            },
            true,
        );
    }
    if file_contradicts_account_uuid {
        // Strip the unevidenced uuid back to this account's name-based
        // identity rather than keeping a `Verified` key nothing supports.
        // The stale entry then simply is not found, so `/admin/pool` omits
        // the plan instead of showing the previous account's.
        let mut unevidenced = account.clone();
        unevidenced.uuid = None;
        return (account_key(upstream, &unevidenced), false);
    }
    let exact = matches!(key.identity, AccountStateIdentity::Verified { .. });
    (key, exact)
}

/// Keyed by [`AccountKey`] — the pool's own stable-identity scheme, shared
/// with `crate::accounts` and the pool health map — rather than by
/// credential content, so a token refresh never invalidates the cache entry.
/// [`plan_cache_key`], not `account_key`, builds the key used here: it
/// prefers the credential file's own `shuntAccountUuid` over `account.uuid`,
/// so the entry is filed under the identity of the credential the plan was
/// actually read from.
///
/// A name-based key remains possible when no uuid exists anywhere. Such a key
/// is *stable* — it never changes spuriously — but it is not *unique across
/// time*: the same name re-provisioned to a different account produces a
/// byte-identical key. Stability is not identity, and nothing in production
/// clears this cache, so that residual case is bounded by
/// [`CachedProfilePlan::ttl`] rather than by the key.
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

fn store_profile_plan(key: AccountKey, exact_identity: bool, resolved: Option<(String, bool)>) {
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
            exact_identity,
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
///
/// Calling it at the start is necessary but **not sufficient**: `cargo test`
/// runs this module's tests concurrently in that one process, so a neighbour
/// calling this mid-test wipes an entry the running test just cached. Tests
/// therefore take it through `tests::exclusive_profile_cache`, which holds a
/// guard for the whole test rather than only clearing at the start.
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
    tokens: &[Option<String>],
    credential_uuids: &[Option<String>],
    read_failed: &[bool],
    file_phase_timed_out: bool,
    budgets: &BackfillBudgets,
    deadline: tokio::time::Instant,
    plans: &mut [Option<String>],
) {
    // Two tiers, attempted in order: an account with no known plan at all
    // goes first, so a refinement candidate (one that already has a
    // file-derived plan and is only asking for a more precise value) never
    // consumes the budget a plan-less account needs first — within this one
    // call. `deadline` is shared across every provider in the outer loop
    // (`src/admin/mod.rs`), so this ordering gives no such guarantee across
    // multiple `claude_oauth` providers in the same request; see the doc
    // comment above for why that starvation is one-time and self-corrects.
    // Every collection here is indexed, never keyed by `account.name`: two
    // resolved accounts may share a display name (see `FileDerivedPlans`),
    // and keying tokens by name would probe one account with the other's
    // bearer token.
    // One key per account. When the file phase timed out, `credential_uuids`
    // is all-absent; the path-keyed memo then supplies the uuid a previous
    // read established, so an account whose identity lives only in its
    // credential file can still find its cached entry.
    let keys: Vec<(AccountKey, bool)> = accounts
        .iter()
        .enumerate()
        .map(|(index, account)| {
            let uuid = credential_uuids[index].clone().or_else(|| {
                // Only when no read happened at all. A completed read that
                // found no uuid is authoritative: the account holding this
                // path today has no identity, whatever an earlier one had.
                if !file_phase_timed_out {
                    return None;
                }
                credential_path(AuthMode::ClaudeOauth, account)
                    .and_then(|path| recall_credential_uuid(&path))
            });
            // The same authority, applied to `account.uuid`. For an inline
            // account `resolve_pool_accounts` derives that field from this
            // very file and memoizes it for the process lifetime, so a value
            // still sitting there after the file itself became unreadable
            // came from an *earlier* read -- of a file since removed,
            // corrupted, or re-provisioned. Keying off it would serve the
            // path's previous occupant's plan for the full exact-identity
            // day.
            //
            // Only an actual read *failure* counts. A file that parses and
            // simply carries no `shuntAccountUuid` is a healthy hand-placed
            // credential, and an operator's `uuid` on it is the legitimate
            // identity -- stripping that would lose the exact key for a valid
            // configuration. A timed-out phase records no failure either, so
            // its fallback also stands.
            let file_contradicts_account_uuid = read_failed[index];
            plan_cache_key(
                upstream,
                account,
                uuid.as_deref(),
                file_contradicts_account_uuid,
            )
        })
        .collect();
    // A non-exact key is only a display name, so two accounts in this one
    // resolved list can share it. The shared process-wide cache cannot tell
    // them apart, and using it would serve the first account's profile result
    // as the second's -- so an ambiguous key is neither read nor written, and
    // each such account is resolved fresh.
    let mut name_key_counts: HashMap<&AccountKey, usize> = HashMap::new();
    for (key, exact) in &keys {
        if !exact {
            *name_key_counts.entry(key).or_insert(0) += 1;
        }
    }
    let ambiguous: HashSet<AccountKey> = name_key_counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(key, _)| key.clone())
        .collect();

    let mut new_candidates: Vec<Candidate> = Vec::new();
    let mut refinement_candidates: Vec<Candidate> = Vec::new();
    for index in 0..accounts.len() {
        let (key, exact_identity) = keys[index].clone();
        let cacheable = !ambiguous.contains(&key);
        if cacheable {
            if let Some(cached) = cached_profile_plan(&key) {
                merge_profile_plan(plans, index, cached);
                continue;
            }
        }
        if tokens[index].is_some() {
            let candidate = Candidate {
                index,
                key,
                exact_identity,
                cacheable,
            };
            if plans[index].is_some() {
                refinement_candidates.push(candidate);
            } else {
                new_candidates.push(candidate);
            }
        }
    }
    let candidates: Vec<Candidate> = new_candidates
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

    for Candidate {
        index,
        key,
        exact_identity,
        cacheable,
    } in candidates
    {
        // Display name only — used for log lines, never as a lookup key.
        let account = &accounts[index];
        // Double-check: another caller may have resolved and cached this
        // exact account while this caller waited for the lock.
        if cacheable {
            if let Some(cached) = cached_profile_plan(&key) {
                merge_profile_plan(plans, index, cached);
                continue;
            }
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining < budgets.min_slice {
            continue;
        }

        // `tokens` guaranteed this account an entry when it became a
        // candidate above; nothing between then and now removes it.
        let Some(access_token) = tokens[index].as_deref() else {
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
                if cacheable {
                    store_profile_plan(key, exact_identity, Some(resolved.clone()));
                }
                merge_profile_plan(plans, index, Some(resolved));
            }
            Ok(None) => {
                tracing::debug!(
                    provider = upstream,
                    account = %account.name,
                    "admin pool: profile backfill fetch failed"
                );
                if cacheable {
                    store_profile_plan(key, exact_identity, None);
                }
            }
            Err(_) => {
                tracing::debug!(
                    provider = upstream,
                    account = %account.name,
                    timeout_ms = attempt_budget.as_millis() as u64,
                    "admin pool: profile backfill attempt timed out"
                );
                if cacheable {
                    store_profile_plan(key, exact_identity, None);
                }
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
fn merge_profile_plan(plans: &mut [Option<String>], index: usize, profile: Option<(String, bool)>) {
    let Some((plan, from_tier)) = profile else {
        return;
    };
    if from_tier || plans[index].is_none() {
        plans[index] = Some(plan);
    }
}

/// One account queued for a live profile fetch, identified by its position
/// in the resolved `accounts` slice — the only unique identifier available
/// (see [`FileDerivedPlans`]).
struct Candidate {
    index: usize,
    key: AccountKey,
    exact_identity: bool,
    /// `false` when this account's key is a display name shared with another
    /// account in the same resolved list -- the shared cache cannot tell the
    /// two apart, so such an account is resolved fresh and never cached.
    cacheable: bool,
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

    /// Serializes every test that touches process-wide backfill state, and
    /// clears the profile cache once the guard is held.
    ///
    /// Two statics are shared by the whole test binary: the profile cache
    /// behind [`reset_profile_cache`], and `BACKFILL_LOCK`. `cargo test` runs
    /// this module's tests concurrently in one process, so without this guard
    /// a neighbour's `reset_profile_cache()` wipes the entry a test just
    /// cached (the test then reads `None` where it expects a cache hit), and
    /// a neighbour holding `BACKFILL_LOCK` can starve a test's budget until
    /// it skips its own probe (the test then counts fewer requests than it
    /// expects). Both surface as assertion failures unrelated to the
    /// behaviour under test.
    ///
    /// `tokio::sync::Mutex` is not poisoned by a panicking holder, so one
    /// failing test does not cascade into every later one.
    static PROFILE_CACHE_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn exclusive_profile_cache() -> tokio::sync::MutexGuard<'static, ()> {
        let guard = PROFILE_CACHE_GUARD.lock().await;
        reset_profile_cache();
        guard
    }

    /// A single-flight lock private to one test.
    ///
    /// Tests must not share the production [`file_read_lock`]: it serializes
    /// every credential read process-wide, so a test whose assertion depends
    /// on its own file phase completing can be starved past its deadline by
    /// whatever else is running concurrently. A starved phase then falls back
    /// to the remembered identity exactly as designed -- which inverts the
    /// assertion under test and reads as a flake. A test that *wants* the
    /// timeout takes the opposite route and holds its own lock across the
    /// call (see [`plans_for_accounts_under`]).
    fn fresh_file_lock() -> Arc<tokio::sync::Mutex<()>> {
        Arc::new(tokio::sync::Mutex::new(()))
    }
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
            &fresh_file_lock(),
            AuthMode::ClaudeOauth,
            &accounts,
            &BackfillBudgets::default(),
            deadline,
        )
        .await
        .expect("file phase must not time out against a 5s deadline");
        assert_eq!(result.plans[0].as_deref(), Some("max"));
        assert_eq!(result.plans[1], None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `resolve_pool_accounts` appends configured accounts after scoped
    /// store entries without deduplicating display names
    /// (`auth::shared::resolve_pool_accounts`), so one provider's resolved
    /// list can legitimately hold two distinct accounts called the same
    /// thing. Results must stay positional: keyed by name, the second
    /// account's file plan overwrites the first's and both rows then show
    /// the same subscription.
    #[tokio::test]
    async fn file_derived_plans_keeps_same_named_accounts_distinct() {
        let dir = unique_test_dir("same-name-plans");
        std::fs::create_dir_all(&dir).unwrap();
        let first_path = dir.join("first.json");
        std::fs::write(
            &first_path,
            serde_json::json!({"claudeAiOauth": {"subscriptionType": "pro"}}).to_string(),
        )
        .unwrap();
        let second_path = dir.join("second.json");
        std::fs::write(
            &second_path,
            serde_json::json!({"claudeAiOauth": {"subscriptionType": "max"}}).to_string(),
        )
        .unwrap();

        // Same `name`, different credentials -- the shape an ordered upstream
        // produces when a scoped store reference and an inline account
        // happen to share a label.
        let accounts = vec![
            AccountConfig {
                name: "collide".to_string(),
                credentials: Some(first_path.to_string_lossy().into_owned()),
                ..Default::default()
            },
            AccountConfig {
                name: "collide".to_string(),
                credentials: Some(second_path.to_string_lossy().into_owned()),
                ..Default::default()
            },
        ];

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let result = file_derived_plans(
            &fresh_file_lock(),
            AuthMode::ClaudeOauth,
            &accounts,
            &BackfillBudgets::default(),
            deadline,
        )
        .await
        .expect("file phase must not time out against a 5s deadline");

        assert_eq!(
            result.plans[0].as_deref(),
            Some("pro"),
            "the first account must keep its own file-derived plan"
        );
        assert_eq!(
            result.plans[1].as_deref(),
            Some("max"),
            "the second account must keep its own file-derived plan"
        );

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
            &fresh_file_lock(),
            AuthMode::KimiOauth,
            &accounts,
            &BackfillBudgets::default(),
            deadline,
        )
        .await
        .expect("unsupported family short-circuits before any timeout is possible");
        // Length-aligned to `accounts`, so "no results" is all-absent
        // rather than an empty vector.
        assert_eq!(result.plans, vec![None]);
        assert_eq!(result.tokens, vec![None]);
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

        let _cache = exclusive_profile_cache().await;
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
        let plans = plans_for_accounts_under(
            &fresh_file_lock(),
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
            plans[0].is_none(),
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

        let _cache = exclusive_profile_cache().await;
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
        let plans = plans_for_accounts_under(
            &fresh_file_lock(),
            AuthMode::ClaudeOauth,
            "starved-test-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &starved_budgets,
            deadline,
        )
        .await;
        assert!(plans[0].is_none());
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
        let plans = plans_for_accounts_under(
            &fresh_file_lock(),
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
            plans[0].as_deref(),
            Some("max"),
            "a starved skip must not poison the cache with a false failure"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn backfill_single_flights_concurrent_calls_for_the_same_account() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _cache = exclusive_profile_cache().await;
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

        // One lock shared by both concurrent calls -- still private to this
        // test, and the file read releases it long before the profile fetch
        // this test is actually single-flighting.
        let lock = fresh_file_lock();
        let (a, b) = tokio::join!(
            plans_for_accounts_under(
                &lock,
                AuthMode::ClaudeOauth,
                "concurrent-test-upstream",
                &server_uri,
                &client,
                std::slice::from_ref(&account),
                &budgets,
                deadline,
            ),
            plans_for_accounts_under(
                &lock,
                AuthMode::ClaudeOauth,
                "concurrent-test-upstream",
                &server_uri,
                &client,
                std::slice::from_ref(&account),
                &budgets,
                deadline,
            ),
        );
        assert_eq!(a[0].as_deref(), Some("max"));
        assert_eq!(b[0].as_deref(), Some("max"));
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

        let _cache = exclusive_profile_cache().await;
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
        let plans = plans_for_accounts_under(
            &fresh_file_lock(),
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
            plans[0].as_deref(),
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

        let _cache = exclusive_profile_cache().await;
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
        let plans = plans_for_accounts_under(
            &fresh_file_lock(),
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
            plans[0].as_deref(),
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
        let _cache = exclusive_profile_cache().await;
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
            &fresh_file_lock(),
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
        assert_eq!(result.plans[0].as_deref(), Some("max"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn plans_for_accounts_shares_one_deadline_across_multiple_providers() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _cache = exclusive_profile_cache().await;

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

        let _first_plans = plans_for_accounts_under(
            &fresh_file_lock(),
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

        let second_plans = plans_for_accounts_under(
            &fresh_file_lock(),
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
            second_plans[0].as_deref(),
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

        let _cache = exclusive_profile_cache().await;
        let dir = unique_test_dir("cache-harvest");
        std::fs::create_dir_all(&dir).unwrap();
        // The file phase is made to time out by holding `lock` across the
        // second call, not by racing the read against the deadline -- see
        // `plans_for_accounts_under`. So an ordinary fixture is enough here.
        let creds_path = write_padded_refreshable_fixture(&dir, "cache-harvest-account", None, 0);
        let lock: Arc<tokio::sync::Mutex<()>> = Arc::new(tokio::sync::Mutex::new(()));
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
        let first_plans = plans_for_accounts_under(
            &lock,
            AuthMode::ClaudeOauth,
            "cache-harvest-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &first_budgets,
            first_deadline,
        )
        .await;
        assert_eq!(first_plans[0].as_deref(), Some("max"));

        // Second call: same upstream and same uuid, so the same AccountKey
        // hits the warm cache -- but the single-flight permit is held here,
        // min_slice is zeroed out (defeating the floor entirely) and the
        // deadline is already expired, so `acquire_read_permit` cannot
        // acquire and the file phase returns `None` with certainty. The
        // cache-hit check inside backfill_claude_profile_plans runs before
        // any token-eligibility check, so the plan must still surface purely
        // from cache, with no additional network request.
        let held = lock.clone().lock_owned().await;
        let second_budgets = BackfillBudgets {
            total: Duration::from_secs(8),
            per_account: Duration::from_secs(5),
            min_slice: Duration::ZERO,
        };
        let second_deadline = tokio::time::Instant::now() - Duration::from_secs(1);
        let second_plans = plans_for_accounts_under(
            &lock,
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
            second_plans[0].as_deref(),
            Some("max"),
            "a cached plan must surface even when the file phase times out and min_slice is zeroed"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "the second call must be a pure cache harvest with zero additional network requests"
        );
        drop(held);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A refreshable fixture with a caller-chosen access token and optional
    /// `shuntAccountUuid`, so a test can tell two accounts' credentials apart
    /// by what reaches the profile endpoint.
    fn write_identified_fixture(
        path: &std::path::Path,
        access_token: &str,
        account_uuid: Option<&str>,
    ) {
        let expires_at_ms = (std::time::SystemTime::now() + Duration::from_secs(3600))
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let mut blob = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access_token,
                "refreshToken": "refresh-token",
                "expiresAt": expires_at_ms
            }
        });
        if let Some(uuid) = account_uuid {
            blob["shuntAccountUuid"] = serde_json::Value::String(uuid.to_string());
        }
        std::fs::write(path, blob.to_string()).unwrap();
    }

    /// The sharper half of the same-name hazard: `tokens` keyed by display
    /// name hands whichever credential was read last to *every* account
    /// sharing that name, so one account's profile is fetched with another
    /// account's bearer token. Each account must be probed with its own.
    #[tokio::test]
    async fn backfill_probes_each_same_named_account_with_its_own_token() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _cache = exclusive_profile_cache().await;
        let dir = unique_test_dir("same-name-tokens");
        std::fs::create_dir_all(&dir).unwrap();
        let first_path = dir.join("first.json");
        write_identified_fixture(&first_path, "token-first", None);
        let second_path = dir.join("second.json");
        write_identified_fixture(&second_path, "token-second", None);

        // Distinct uuids keep the two cache keys apart, so both accounts
        // genuinely reach the fetch and the tokens they carry are what this
        // test observes. The *names* still collide, which is the hazard.
        let accounts = vec![
            AccountConfig {
                name: "collide".to_string(),
                uuid: Some("uuid-first".to_string()),
                credentials: Some(first_path.to_string_lossy().into_owned()),
                ..Default::default()
            },
            AccountConfig {
                name: "collide".to_string(),
                uuid: Some("uuid-second".to_string()),
                credentials: Some(second_path.to_string_lossy().into_owned()),
                ..Default::default()
            },
        ];

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/profile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "organization": {"organization_type": "claude_max"}
            })))
            .mount(&server)
            .await;

        let budgets = BackfillBudgets::default();
        let deadline = tokio::time::Instant::now() + budgets.total;
        let client = reqwest::Client::new();
        let _plans = plans_for_accounts_under(
            &fresh_file_lock(),
            AuthMode::ClaudeOauth,
            "same-name-token-upstream",
            &server.uri(),
            &client,
            &accounts,
            &budgets,
            deadline,
        )
        .await;

        let mut seen: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter_map(|request| {
                request
                    .headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned)
            })
            .collect();
        seen.sort();
        assert_eq!(
            seen,
            vec![
                "Bearer token-first".to_string(),
                "Bearer token-second".to_string()
            ],
            "each account must be probed with the token from its own credential file; a \
             name-keyed token map sends the last-read token twice"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Nothing in production clears the profile cache, so a uuid-less account
    /// re-provisioned under the same store name must not be served the
    /// previous account's plan. The credential file's own `shuntAccountUuid`
    /// is what distinguishes them.
    #[tokio::test]
    async fn reprovisioning_under_the_same_name_is_not_served_the_previous_plan() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _cache = exclusive_profile_cache().await;
        let dir = unique_test_dir("reprovision");
        std::fs::create_dir_all(&dir).unwrap();
        let creds_path = dir.join("reprovision.json");
        write_identified_fixture(&creds_path, "token-before", Some("uuid-before"));

        // No `uuid` on the config: without the credential file's own
        // identity this keys as `UpstreamInline { upstream, name }`, which is
        // byte-identical before and after re-provisioning.
        let account = AccountConfig {
            name: "reprovisioned".to_string(),
            credentials: Some(creds_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/profile"))
            .and(header("authorization", "Bearer token-before"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "organization": {"rate_limit_tier": "default_claude_max_20x"}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/profile"))
            .and(header("authorization", "Bearer token-after"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "organization": {"rate_limit_tier": "default_claude_pro_5x"}
            })))
            .mount(&server)
            .await;

        let budgets = BackfillBudgets::default();
        let client = reqwest::Client::new();
        let first = plans_for_accounts_under(
            &fresh_file_lock(),
            AuthMode::ClaudeOauth,
            "reprovision-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &budgets,
            tokio::time::Instant::now() + budgets.total,
        )
        .await;
        assert_eq!(first[0].as_deref(), Some("max 20x"));

        // Same name, same path -- a different account.
        write_identified_fixture(&creds_path, "token-after", Some("uuid-after"));
        let second = plans_for_accounts_under(
            &fresh_file_lock(),
            AuthMode::ClaudeOauth,
            "reprovision-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &budgets,
            tokio::time::Instant::now() + budgets.total,
        )
        .await;

        assert_eq!(
            second[0].as_deref(),
            Some("pro 5x"),
            "the replacement account's own plan must be resolved, not the cached plan of the \
             account that previously held this name"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "the replacement account must reach the profile endpoint rather than hit the \
             previous account's cache entry"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stale_account_uuid_does_not_pin_the_cache_to_the_previous_occupant() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _cache = exclusive_profile_cache().await;
        let dir = unique_test_dir("stale-uuid");
        std::fs::create_dir_all(&dir).unwrap();
        let creds_path = dir.join("stale-uuid.json");
        write_identified_fixture(&creds_path, "token-before", Some("uuid-before"));

        // `uuid` carries the *previous* occupant of this path. That is exactly
        // what `resolve_pool_accounts` hands us after a re-provisioning with no
        // config reload: its inline-identity memo is process-lifetime, so it
        // keeps answering "uuid-before" while the file already says
        // "uuid-after" (`auth::shared`). An operator's stale `uuid = ` entry
        // reaches this code identically.
        let account = AccountConfig {
            name: "stale".to_string(),
            uuid: Some("uuid-before".to_string()),
            credentials: Some(creds_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/profile"))
            .and(header("authorization", "Bearer token-before"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "organization": {"rate_limit_tier": "default_claude_max_20x"}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/profile"))
            .and(header("authorization", "Bearer token-after"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "organization": {"rate_limit_tier": "default_claude_pro_5x"}
            })))
            .mount(&server)
            .await;

        let budgets = BackfillBudgets::default();
        let client = reqwest::Client::new();
        let first = plans_for_accounts_under(
            &fresh_file_lock(),
            AuthMode::ClaudeOauth,
            "stale-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &budgets,
            tokio::time::Instant::now() + budgets.total,
        )
        .await;
        assert_eq!(first[0].as_deref(), Some("max 20x"));

        // The path is re-provisioned to a different account. `account.uuid`
        // does not move -- only the file does.
        write_identified_fixture(&creds_path, "token-after", Some("uuid-after"));
        let second = plans_for_accounts_under(
            &fresh_file_lock(),
            AuthMode::ClaudeOauth,
            "stale-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &budgets,
            tokio::time::Instant::now() + budgets.total,
        )
        .await;
        assert_eq!(
            second[0].as_deref(),
            Some("pro 5x"),
            "keying off the stale `account.uuid` would file the new account's plan under the old \
             account's identity and keep serving `max 20x` for the 24h exact-identity TTL"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_cache_key_prefers_the_credential_file_uuid() {
        let account = AccountConfig {
            name: "named".to_string(),
            uuid: Some("stale-uuid".to_string()),
            ..Default::default()
        };
        let (key, exact) = plan_cache_key("upstream", &account, Some("credential-uuid"), false);
        assert!(exact);
        assert_eq!(
            key.identity,
            AccountStateIdentity::Verified {
                id: "credential-uuid".to_string()
            },
            "the plan belongs to whoever the file's token authenticates as, so the file's own \
             uuid outranks the label on the account -- which may be a stale operator entry or a \
             stale inline-identity memo naming the path's previous occupant"
        );
    }

    #[test]
    fn plan_cache_key_keeps_an_account_uuid_the_file_did_not_contradict() {
        let account = AccountConfig {
            name: "named".to_string(),
            uuid: Some("configured-uuid".to_string()),
            ..Default::default()
        };
        let (key, exact) = plan_cache_key("upstream", &account, None, false);
        assert!(exact, "a uuid is an identity when nothing contradicts it");
        assert_eq!(
            key.identity,
            AccountStateIdentity::Verified {
                id: "configured-uuid".to_string()
            },
            "with no uuid from the file -- because none was read, or none was expected -- the \
             account's own is the best identity available"
        );
    }

    #[test]
    fn plan_cache_key_drops_an_account_uuid_the_file_contradicted() {
        let account = AccountConfig {
            name: "named".to_string(),
            uuid: Some("memoized-uuid".to_string()),
            ..Default::default()
        };
        let (key, exact) = plan_cache_key("upstream", &account, None, true);
        assert!(!exact, "an unevidenced uuid is not an identity");
        assert_eq!(
            key.identity,
            AccountStateIdentity::UpstreamInline {
                upstream: "upstream".to_string(),
                name: "named".to_string()
            },
            "a completed read that produced no uuid is evidence against the one still on the \
             account -- keying off it would find the previous occupant's cached plan"
        );
    }

    #[test]
    fn plan_cache_key_upgrades_a_name_key_with_the_credential_uuid() {
        let account = AccountConfig {
            name: "named".to_string(),
            ..Default::default()
        };
        let (name_keyed, exact_without) = plan_cache_key("upstream", &account, None, false);
        assert!(!exact_without, "a bare name is not an identity");
        assert_eq!(
            name_keyed.identity,
            AccountStateIdentity::UpstreamInline {
                upstream: "upstream".to_string(),
                name: "named".to_string()
            }
        );

        let (upgraded, exact_with) =
            plan_cache_key("upstream", &account, Some("credential-uuid"), false);
        assert!(exact_with);
        assert_eq!(
            upgraded.identity,
            AccountStateIdentity::Verified {
                id: "credential-uuid".to_string()
            },
            "the credential file's own uuid must key the entry when config carries none"
        );
    }

    #[test]
    fn a_name_keyed_success_is_not_cached_for_a_day() {
        let exact = CachedProfilePlan {
            plan: Some("max".to_string()),
            from_tier: true,
            fetched_at: Instant::now(),
            exact_identity: true,
        };
        let name_only = CachedProfilePlan {
            exact_identity: false,
            ..CachedProfilePlan {
                plan: Some("max".to_string()),
                from_tier: true,
                fetched_at: Instant::now(),
                exact_identity: true,
            }
        };
        assert_eq!(exact.ttl(), Duration::from_secs(24 * 60 * 60));
        assert_eq!(
            name_only.ttl(),
            Duration::from_secs(10 * 60),
            "a key that is only a display name must not pin a plan for a day -- the same name \
             may belong to a different account by then"
        );
    }

    /// The give-up path that bounds the blocking-pool leak: when a previous
    /// read still holds the permit, a new request must return without
    /// spawning a second read. Exercised against a local lock -- holding the
    /// process-wide one would fail every concurrently running test's
    /// credential read.
    #[tokio::test]
    async fn read_permit_is_refused_while_an_earlier_read_holds_it() {
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let held = lock.clone().lock_owned().await;

        let refused = acquire_read_permit(
            &lock,
            tokio::time::Instant::now() + Duration::from_millis(20),
        )
        .await;
        assert!(
            refused.is_none(),
            "a request must give up rather than queue behind a read that may never finish"
        );

        drop(held);
        let granted =
            acquire_read_permit(&lock, tokio::time::Instant::now() + Duration::from_secs(5)).await;
        assert!(
            granted.is_some(),
            "the permit must be available again once the holder releases it"
        );
    }

    /// The complement: a read that completes must hand the permit back, or
    /// the first `/admin/pool` request would wedge every later one.
    #[tokio::test]
    async fn read_permit_is_released_after_a_completed_read() {
        let dir = unique_test_dir("permit-release");
        std::fs::create_dir_all(&dir).unwrap();
        let creds_path = dir.join("released.json");
        std::fs::write(
            &creds_path,
            serde_json::json!({"claudeAiOauth": {"subscriptionType": "max"}}).to_string(),
        )
        .unwrap();
        let accounts = vec![AccountConfig {
            name: "released".to_string(),
            credentials: Some(creds_path.to_string_lossy().into_owned()),
            ..Default::default()
        }];

        // Driven against a lock of this test's own: asserting on the
        // process-wide one is order-dependent, since a concurrently running
        // test may legitimately be holding it.
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let result = file_derived_plans(
            &lock,
            AuthMode::ClaudeOauth,
            &accounts,
            &BackfillBudgets::default(),
            tokio::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect("a local file read must not time out against a 5s deadline");
        assert_eq!(result.plans[0].as_deref(), Some("max"));

        assert!(
            lock.try_lock().is_ok(),
            "the permit must be released once the blocking read finishes"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
    /// A stalled file phase must not lose access to plans an earlier
    /// resolution already cached. The identity of a uuid-less account lives
    /// only in its credential file, so when that file cannot be read the
    /// path-keyed memo is the only way back to its cache entry.
    #[tokio::test]
    async fn a_timed_out_file_phase_still_serves_a_cached_plan_for_a_uuid_less_account() {
        use wiremock::matchers::{method, path as path_matcher};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _cache = exclusive_profile_cache().await;
        let dir = unique_test_dir("timeout-harvest");
        std::fs::create_dir_all(&dir).unwrap();
        let creds_path = dir.join("harvest.json");
        write_identified_fixture(&creds_path, "token-harvest", Some("uuid-harvest"));
        // The file phase is made to time out by holding `lock` across the
        // second call rather than by racing the read against the deadline --
        // see `plans_for_accounts_under`. Without that the read may well
        // succeed, the uuid is re-read, and this test silently stops
        // exercising the timeout path it exists to cover.
        let lock: Arc<tokio::sync::Mutex<()>> = Arc::new(tokio::sync::Mutex::new(()));

        // No config uuid: the account's identity is only in the file.
        let account = AccountConfig {
            name: "harvested".to_string(),
            credentials: Some(creds_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/oauth/profile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "organization": {"rate_limit_tier": "default_claude_max_20x"}
            })))
            .mount(&server)
            .await;

        let budgets = BackfillBudgets::default();
        let client = reqwest::Client::new();
        let first = plans_for_accounts_under(
            &lock,
            AuthMode::ClaudeOauth,
            "timeout-harvest-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &budgets,
            tokio::time::Instant::now() + budgets.total,
        )
        .await;
        assert_eq!(first[0].as_deref(), Some("max 20x"));

        // Force the file phase to time out: the permit is held here,
        // `min_slice` zeroed defeats the floor, and the deadline is already
        // expired, so `acquire_read_permit` cannot acquire. No credential is
        // read and no uuid reaches the backfill from this pass.
        let held = lock.clone().lock_owned().await;
        let starved = BackfillBudgets {
            total: Duration::from_secs(8),
            per_account: Duration::from_secs(5),
            min_slice: Duration::ZERO,
        };
        let second = plans_for_accounts_under(
            &lock,
            AuthMode::ClaudeOauth,
            "timeout-harvest-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &starved,
            tokio::time::Instant::now() - Duration::from_secs(1),
        )
        .await;

        assert_eq!(
            second[0].as_deref(),
            Some("max 20x"),
            "a plan cached from an earlier resolution must still appear when the file phase \
             times out, even though the account's uuid could not be re-read"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "the cached plan must be harvested, not re-fetched"
        );
        drop(held);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two accounts with no identity anywhere -- no config uuid, no
    /// `shuntAccountUuid` -- collapse onto one `UpstreamInline` key. The
    /// shared cache cannot separate them, so neither may use it: without the
    /// guard the first account's profile result is served as the second's.
    #[tokio::test]
    async fn ambiguous_name_keys_are_resolved_fresh_rather_than_shared() {
        use wiremock::matchers::{header, method, path as path_matcher};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _cache = exclusive_profile_cache().await;
        let dir = unique_test_dir("ambiguous-key");
        std::fs::create_dir_all(&dir).unwrap();
        let first_path = dir.join("first.json");
        write_identified_fixture(&first_path, "token-alpha", None);
        let second_path = dir.join("second.json");
        write_identified_fixture(&second_path, "token-beta", None);

        let accounts = vec![
            AccountConfig {
                name: "twins".to_string(),
                credentials: Some(first_path.to_string_lossy().into_owned()),
                ..Default::default()
            },
            AccountConfig {
                name: "twins".to_string(),
                credentials: Some(second_path.to_string_lossy().into_owned()),
                ..Default::default()
            },
        ];

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/oauth/profile"))
            .and(header("authorization", "Bearer token-alpha"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "organization": {"rate_limit_tier": "default_claude_max_20x"}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/oauth/profile"))
            .and(header("authorization", "Bearer token-beta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "organization": {"rate_limit_tier": "default_claude_pro_5x"}
            })))
            .mount(&server)
            .await;

        let budgets = BackfillBudgets::default();
        let plans = plans_for_accounts_under(
            &fresh_file_lock(),
            AuthMode::ClaudeOauth,
            "ambiguous-upstream",
            &server.uri(),
            &reqwest::Client::new(),
            &accounts,
            &budgets,
            tokio::time::Instant::now() + budgets.total,
        )
        .await;

        assert_eq!(
            (plans[0].as_deref(), plans[1].as_deref()),
            (Some("max 20x"), Some("pro 5x")),
            "each account must get its own profile result; a shared name key serves the first \
             account's plan to the second"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
    /// The memo must never outlive the identity it recorded. A credential
    /// path that once held uuid `U` and is later read back successfully
    /// *without* one belongs to a different, identity-less account: recalling
    /// `U` would hit the old account's `Verified` cache entry and skip the
    /// replacement's own lookup entirely.
    #[tokio::test]
    async fn a_completed_uuid_less_read_does_not_recall_the_previous_identity() {
        use wiremock::matchers::{header, method, path as path_matcher};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _cache = exclusive_profile_cache().await;
        let dir = unique_test_dir("memo-invalidate");
        std::fs::create_dir_all(&dir).unwrap();
        let creds_path = dir.join("swapped.json");
        write_identified_fixture(&creds_path, "token-identified", Some("uuid-identified"));

        let account = AccountConfig {
            name: "swapped".to_string(),
            credentials: Some(creds_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/oauth/profile"))
            .and(header("authorization", "Bearer token-identified"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "organization": {"rate_limit_tier": "default_claude_max_20x"}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/oauth/profile"))
            .and(header("authorization", "Bearer token-anonymous"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "organization": {"rate_limit_tier": "default_claude_pro_5x"}
            })))
            .mount(&server)
            .await;

        let budgets = BackfillBudgets::default();
        let client = reqwest::Client::new();
        let first = plans_for_accounts_under(
            &fresh_file_lock(),
            AuthMode::ClaudeOauth,
            "memo-invalidate-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &budgets,
            tokio::time::Instant::now() + budgets.total,
        )
        .await;
        assert_eq!(first[0].as_deref(), Some("max 20x"));

        // Same path, a different account -- and this one carries no
        // `shuntAccountUuid` at all.
        write_identified_fixture(&creds_path, "token-anonymous", None);
        let second = plans_for_accounts_under(
            &fresh_file_lock(),
            AuthMode::ClaudeOauth,
            "memo-invalidate-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &budgets,
            tokio::time::Instant::now() + budgets.total,
        )
        .await;

        assert_eq!(
            second[0].as_deref(),
            Some("pro 5x"),
            "a successfully read uuid-less credential must resolve its own plan, not inherit \
             the cached plan of the identified account that previously used this path"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "the replacement must reach the profile endpoint rather than hit the remembered \
             identity's cache entry"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
    /// The half of the guard that `forget_credential_uuid` cannot cover: an
    /// unreadable or unparsable credential file never reaches the forget
    /// call (the read loop skips it), so only the "did the phase actually
    /// time out" gate stops the stale identity from being recalled. Without
    /// that gate the account's cache entry is served for a credential that
    /// can no longer be read at all.
    /// The sibling hazard to `a_failed_read_clears_the_remembered_identity`,
    /// on the *other* memo. Clearing the path memo is not enough when
    /// `account.uuid` itself was filled from `resolve_pool_accounts`'"'"'s
    /// process-lifetime inline-identity memo: that value survives the
    /// credential file it came from, so the cache key would still be built
    /// from the previous occupant'"'"'s identity.
    #[tokio::test]
    async fn a_completed_failed_read_does_not_key_off_a_memoized_account_uuid() {
        use wiremock::matchers::{method, path as path_matcher};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _cache = exclusive_profile_cache().await;
        let dir = unique_test_dir("memoized-uuid");
        std::fs::create_dir_all(&dir).unwrap();
        let creds_path = dir.join("memoized.json");
        write_identified_fixture(&creds_path, "token-memoized", Some("uuid-memoized"));

        // `uuid` as `resolve_pool_accounts` leaves it for a warm inline
        // account: derived from this very file on an earlier request and
        // memoized for the process lifetime, so it does not move when the
        // file does.
        let account = AccountConfig {
            name: "memoized".to_string(),
            uuid: Some("uuid-memoized".to_string()),
            credentials: Some(creds_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/oauth/profile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "organization": {"rate_limit_tier": "default_claude_max_20x"}
            })))
            .mount(&server)
            .await;

        let budgets = BackfillBudgets::default();
        let client = reqwest::Client::new();
        let first = plans_for_accounts_under(
            &fresh_file_lock(),
            AuthMode::ClaudeOauth,
            "memoized-uuid-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &budgets,
            tokio::time::Instant::now() + budgets.total,
        )
        .await;
        assert_eq!(first[0].as_deref(), Some("max 20x"));

        // The credential is gone. This read *completes* and produces no uuid,
        // so nothing recalls the path memo -- but `account.uuid` still names
        // the account that used to live here.
        std::fs::remove_file(&creds_path).unwrap();
        let second = plans_for_accounts_under(
            &fresh_file_lock(),
            AuthMode::ClaudeOauth,
            "memoized-uuid-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &budgets,
            tokio::time::Instant::now() + budgets.total,
        )
        .await;
        assert_eq!(
            second[0], None,
            "a completed read that produced no uuid is evidence against `account.uuid`; keying \
             off it anyway serves the plan cached under the identity this path no longer holds"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The sharper half of `an_unreadable_credential_does_not_recall_the_...`:
    /// that one shows a *completed* read is not served the stale identity,
    /// which the timeout gate alone achieves. This one shows the memo is
    /// actually cleared, by making a *later* request time out -- the one path
    /// that does consult it.
    #[tokio::test]
    async fn a_failed_read_clears_the_remembered_identity() {
        use wiremock::matchers::{method, path as path_matcher};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _cache = exclusive_profile_cache().await;
        let dir = unique_test_dir("memo-cleared");
        std::fs::create_dir_all(&dir).unwrap();
        let creds_path = dir.join("cleared.json");
        write_identified_fixture(&creds_path, "token-readable", Some("uuid-readable"));

        let account = AccountConfig {
            name: "cleared".to_string(),
            credentials: Some(creds_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/oauth/profile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "organization": {"rate_limit_tier": "default_claude_max_20x"}
            })))
            .mount(&server)
            .await;

        let budgets = BackfillBudgets::default();
        let client = reqwest::Client::new();
        let lock = fresh_file_lock();

        // Resolve once: the plan is cached under `Verified { uuid-readable }`
        // and the path remembers that uuid.
        let first = plans_for_accounts_under(
            &lock,
            AuthMode::ClaudeOauth,
            "memo-cleared-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &budgets,
            tokio::time::Instant::now() + budgets.total,
        )
        .await;
        assert_eq!(first[0].as_deref(), Some("max 20x"));

        // A completed read that cannot parse the file. Nothing is recorded
        // for this path -- and the memo entry must be dropped.
        std::fs::remove_file(&creds_path).unwrap();
        let _second = plans_for_accounts_under(
            &lock,
            AuthMode::ClaudeOauth,
            "memo-cleared-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &budgets,
            tokio::time::Instant::now() + budgets.total,
        )
        .await;

        // Now force the recall path: holding the permit with an expired
        // deadline makes the file phase time out with certainty, which is the
        // only path that consults the memo.
        let held = lock.clone().lock_owned().await;
        let starved = BackfillBudgets {
            total: Duration::from_secs(8),
            per_account: Duration::from_secs(5),
            min_slice: Duration::ZERO,
        };
        let third = plans_for_accounts_under(
            &lock,
            AuthMode::ClaudeOauth,
            "memo-cleared-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &starved,
            tokio::time::Instant::now() - Duration::from_secs(1),
        )
        .await;
        assert_eq!(
            third[0], None,
            "the failed read must have cleared the memo -- otherwise this timed-out phase \
             recalls `uuid-readable` and serves the plan cached under the identity this path \
             no longer holds"
        );
        drop(held);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_unreadable_credential_does_not_recall_the_remembered_identity() {
        use wiremock::matchers::{method, path as path_matcher};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _cache = exclusive_profile_cache().await;
        let dir = unique_test_dir("memo-unreadable");
        std::fs::create_dir_all(&dir).unwrap();
        let creds_path = dir.join("corrupt.json");
        write_identified_fixture(&creds_path, "token-readable", Some("uuid-readable"));

        let account = AccountConfig {
            name: "corrupted".to_string(),
            credentials: Some(creds_path.to_string_lossy().into_owned()),
            ..Default::default()
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_matcher("/api/oauth/profile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "organization": {"rate_limit_tier": "default_claude_max_20x"}
            })))
            .mount(&server)
            .await;

        let budgets = BackfillBudgets::default();
        let client = reqwest::Client::new();
        let first = plans_for_accounts_under(
            &fresh_file_lock(),
            AuthMode::ClaudeOauth,
            "memo-unreadable-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &budgets,
            tokio::time::Instant::now() + budgets.total,
        )
        .await;
        assert_eq!(first[0].as_deref(), Some("max 20x"));

        // Not valid JSON: `read_json` returns `None` and the read loop skips
        // this path without recording or clearing anything for it.
        std::fs::write(&creds_path, b"}{ not json").unwrap();
        let second = plans_for_accounts_under(
            &fresh_file_lock(),
            AuthMode::ClaudeOauth,
            "memo-unreadable-upstream",
            &server.uri(),
            &client,
            std::slice::from_ref(&account),
            &budgets,
            tokio::time::Instant::now() + budgets.total,
        )
        .await;

        assert_eq!(
            second[0], None,
            "an unreadable credential must report no plan, not the plan cached under the \
             identity its path used to hold"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
