use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use reqwest::{header::HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;

use crate::config::{AccountConfig, PoolConfig};

type AccountKey = (String, String);
type RefreshLock = Arc<AsyncMutex<()>>;

/// Legacy hard threshold, used verbatim when `[server.pool]` is not
/// configured so selection behaves exactly as it did before issue #135.
const SWITCH_THRESHOLD: f64 = 0.98;

/// Window lengths are hardcoded because the quota headers carry only the
/// reset instant, never the window size (issue #135).
const WINDOW_5H_SECS: u64 = 5 * 60 * 60;
const WINDOW_7D_SECS: u64 = 7 * 24 * 60 * 60;

/// One quota window for per-window threshold resolution. `Weekly` is the
/// shared `7d` bucket; `Fable` is the fable-only `7d_oi` bucket.
#[derive(Debug, Clone, Copy)]
enum QuotaWindow {
    FiveHour,
    Weekly,
    Fable,
}

/// Dashboard bucket for one Codex rate-limit window. Codex identifies these by
/// duration, not by the primary/secondary header position.
#[derive(Debug, Clone, Copy)]
enum CodexWindow {
    FiveHour,
    Weekly,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QuotaState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utilization_5h: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_5h: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utilization_7d: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_7d: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utilization_7d_oi: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_7d_oi: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

impl QuotaState {
    /// Whether any persisted quota field carries a recorded signal. Utilization,
    /// reset metadata, and unified status all affect selection or diagnostics, so
    /// only an entirely default quota is omitted from persistence.
    pub(crate) fn has_signal(&self) -> bool {
        self.utilization_5h.is_some()
            || self.reset_5h.is_some()
            || self.utilization_7d.is_some()
            || self.reset_7d.is_some()
            || self.utilization_7d_oi.is_some()
            || self.reset_7d_oi.is_some()
            || self.status.is_some()
    }
}

/// One rate-limit window's authoritative usage as reported by the Anthropic
/// OAuth usage API (`GET /api/oauth/usage`). Unlike the per-response
/// `anthropic-ratelimit-unified-*` headers — which only reflect traffic that
/// flowed through shunt — the usage API reports ground-truth utilization that
/// includes out-of-band consumption of the same account (the user's own Claude
/// Code, other tools). See [`AccountPool::note_usage`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageWindow {
    /// Fraction in `0.0..=1.0` (the API's 0–100 percent divided by 100).
    pub utilization: f64,
    /// Reset time in Unix epoch seconds, when the API reports one.
    pub resets_at: Option<u64>,
}

/// Authoritative account usage across the three tracked windows, parsed from the
/// Anthropic OAuth usage API and applied to a pool account via
/// [`AccountPool::note_usage`]. A `None` window means the API did not report that
/// bucket (e.g. no Fable-scoped weekly limit), leaving any prior value in place.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageSnapshot {
    /// The rolling 5-hour session window (all models).
    pub five_hour: Option<UsageWindow>,
    /// The shared weekly window (non-Fable models).
    pub seven_day: Option<UsageWindow>,
    /// The Fable-scoped weekly window (`7d_oi`).
    pub seven_day_oi: Option<UsageWindow>,
}

/// How long an identity must sit with no in-flight requests before storm
/// control drops its admission allowance back to the initial value. An account
/// that was idle this long (typically because the pool's traffic was sticky on
/// another account) re-enters slow start when a failover burst arrives, which
/// is exactly the stampede the gate exists to absorb (issue #195).
const RAMP_IDLE_RESET: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
struct AccountHealth {
    cooldown_until: Option<Instant>,
    quota: QuotaState,
    /// Latest configured selection state. Quota gauges exclude disabled accounts.
    enabled: bool,
    /// Whether the pool has processed at least one upstream response for this
    /// account (a quota update, a cooldown, or a healthy-mark). `select_order`
    /// inserts a default entry on selection, so entry existence alone does not
    /// mean an account has been observed — the admin dashboard's `has_state`
    /// keys off this flag instead of mere entry presence.
    observed: bool,
    /// Requests currently admitted to this identity by [`AccountPool::try_admit`].
    /// Only maintained when storm control is configured; stays `0` otherwise.
    in_flight: u32,
    /// Storm-control slow-start allowance: the number of concurrent admissions
    /// this identity currently accepts. `0` means "restart the ramp" — the next
    /// [`AccountPool::try_admit`] re-seeds it from the configured initial value
    /// (a fresh entry starts there, and [`AccountPool::cooldown`] resets to it).
    ramp_allowance: u32,
    /// Instant of the last admission or release, for the idle-reset rule.
    ramp_last_activity: Option<Instant>,
}

/// Token-free, serializable view of one account's pool health for the admin
/// dashboard (`GET /admin/pool`). Derived from [`AccountHealth`]; see
/// [`AccountPool::snapshot`].
#[derive(Debug, Clone, Serialize)]
pub struct AccountSnapshot {
    pub name: String,
    /// Whether the pool has recorded at least one upstream response for this
    /// account. When `false`, the quota/cooldown fields are all absent.
    pub has_state: bool,
    /// Derived: not disabled, not cooling down, and not near quota.
    pub available: bool,
    pub near_quota: bool,
    /// Seconds until the current cooldown expires, when the account is cooling.
    pub cooldown_secs_remaining: Option<u64>,
    /// Configured selection priority (lower is preferred; default 100).
    pub priority: u32,
    /// Configured exclusion from pool selection.
    pub disabled: bool,
    /// Burn-rate headroom in seconds across the governing quota windows, when
    /// `[server.pool]` is configured and the projection is finite: positive
    /// means the account survives to its tightest reset at the current pace.
    pub headroom_secs: Option<i64>,
    pub utilization_5h: Option<f64>,
    pub reset_5h: Option<u64>,
    pub utilization_7d: Option<f64>,
    pub reset_7d: Option<u64>,
    pub utilization_7d_oi: Option<f64>,
    pub reset_7d_oi: Option<u64>,
    pub status: Option<String>,
}

impl AccountSnapshot {
    /// A clean slot for an account the pool has never selected.
    fn unseen(account: &AccountConfig) -> Self {
        Self {
            name: account.name.clone(),
            has_state: false,
            available: !account.disabled,
            near_quota: false,
            cooldown_secs_remaining: None,
            priority: account.priority,
            disabled: account.disabled,
            headroom_secs: None,
            utilization_5h: None,
            reset_5h: None,
            utilization_7d: None,
            reset_7d: None,
            utilization_7d_oi: None,
            reset_7d_oi: None,
            status: None,
        }
    }
}

/// Process-lifetime health and scheduling state for configured accounts.
#[derive(Debug, Default)]
pub struct AccountPool {
    entries: Mutex<HashMap<AccountKey, AccountHealth>>,
    rr: Mutex<HashMap<String, usize>>,
    refresh_locks: Mutex<HashMap<AccountKey, RefreshLock>>,
    /// Set whenever a quota mutation lands, cleared by [`Self::take_dirty`].
    /// Lets the opt-in on-disk persister (see [`crate::state_persist`]) flush
    /// only when quota actually changed, rather than on every timer tick.
    dirty: AtomicBool,
}

impl AccountPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Synchronize the provider's current configured identities before an
    /// out-of-band usage poll updates individual accounts. An identity stays
    /// enabled when any alias is enabled, and entries removed by a config reload
    /// stop contributing to pool-wide quota metrics.
    pub(crate) fn sync_enabled_accounts(&self, provider: &str, accounts: &[AccountConfig]) {
        let mut entries = self.entries.lock().expect("account health lock poisoned");
        for ((entry_provider, _), health) in entries.iter_mut() {
            if entry_provider == provider {
                health.enabled = false;
            }
        }
        for account in accounts {
            entries
                .entry((provider.to_string(), account_identity(account).to_string()))
                .or_default()
                .enabled |= !account.disabled;
        }
    }

    /// Return account indices in the order an adapter should try them.
    ///
    /// `pool` is the optional `[server.pool]` tuning (issue #135). When
    /// absent, selection is the pre-#135 behavior: a single 0.98 hard
    /// threshold and weekly-reset ordering. When present, available accounts
    /// order by `priority` then burn-rate headroom, soft-threshold-near
    /// accounts fall back to headroom order (the all-near guard), and
    /// accounts past `hard_threshold` sort last among the available.
    /// Per-account `priority`/`disabled` apply in both modes.
    pub fn select_order(
        &self,
        provider: &str,
        accounts: &[AccountConfig],
        session_id: Option<&str>,
        model: Option<&str>,
        pool: Option<&PoolConfig>,
    ) -> Vec<usize> {
        self.select_order_inner(provider, accounts, session_id, model, pool)
    }

    fn select_order_inner(
        &self,
        provider: &str,
        accounts: &[AccountConfig],
        session_id: Option<&str>,
        model: Option<&str>,
        pool: Option<&PoolConfig>,
    ) -> Vec<usize> {
        if accounts.is_empty() {
            return Vec::new();
        }

        let provider = provider.to_string();
        let ident_reps = collapse_representatives(accounts);
        let distinct = ident_reps.len();
        let start_slot = match session_id {
            Some(session_id) => stable_session_index(session_id, distinct),
            None => {
                let mut counters = self.rr.lock().expect("account round-robin lock poisoned");
                let counter = counters.entry(provider.clone()).or_default();
                let start_slot = *counter % distinct;
                *counter = counter.wrapping_add(1);
                start_slot
            }
        };

        let now = Instant::now();
        let unix_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let is_fable = is_fable_model(model);
        let snapshots = {
            let mut entries = self.entries.lock().expect("account health lock poisoned");
            // Treat this selection snapshot as the authoritative enabled set for
            // the provider. Entries left behind by a config reload must not keep
            // contributing to pool-wide quota gauges.
            for ((entry_provider, _), health) in entries.iter_mut() {
                if entry_provider == &provider {
                    health.enabled = false;
                }
            }
            let mut snapshots = Vec::with_capacity(accounts.len());
            for account in accounts {
                let health = entries
                    .entry((provider.clone(), account_identity(account).to_string()))
                    .or_default();
                health.enabled |= !account.disabled;
                expire_stale_quota(&mut health.quota, unix_now);
                // Assessing under the lock is pure CPU work and avoids cloning
                // each account's QuotaState just to assess it after release.
                let assessment = assess_quota(&health.quota, account, is_fable, pool, unix_now);
                let weekly_reset = governing_weekly_reset(&health.quota, is_fable);
                snapshots.push((health.cooldown_until, assessment, weekly_reset));
            }
            snapshots
        };

        // The sticky/round-robin slot is computed over distinct identities so
        // adding or removing an alias cannot move an existing session. Disabled
        // aliases yield to an enabled representative; fully disabled identities
        // are then dropped from the rotation entirely.
        let rotation = (0..distinct)
            .map(|offset| ident_reps[(start_slot + offset) % distinct])
            .filter(|&index| !accounts[index].disabled)
            .collect::<Vec<_>>();
        let sticky = ident_reps[start_slot];
        let (sticky_cooldown, ref sticky_quota, _) = snapshots[sticky];
        if !accounts[sticky].disabled
            && sticky_cooldown.is_none_or(|until| until <= now)
            && !sticky_quota.near
        {
            return rotation;
        }

        let is_available =
            |index: usize| snapshots[index].0.is_none_or(|until: Instant| until <= now);

        let mut available_under = rotation
            .iter()
            .copied()
            .filter(|&index| is_available(index) && !snapshots[index].1.near)
            .collect::<Vec<_>>();
        // The stable sorts below preserve rotation order as the final tiebreak.
        match pool {
            // Priority beats headroom; ties prefer the account projected to
            // keep the most margin before its tightest window resets.
            Some(_) => available_under.sort_by(|&left, &right| {
                accounts[left]
                    .priority
                    .cmp(&accounts[right].priority)
                    .then_with(|| {
                        snapshots[right]
                            .1
                            .headroom
                            .total_cmp(&snapshots[left].1.headroom)
                    })
            }),
            // Legacy: `Option` orders `None` before `Some`, so accounts with
            // an unknown weekly reset sort first.
            None => available_under.sort_by(|&left, &right| {
                accounts[left]
                    .priority
                    .cmp(&accounts[right].priority)
                    .then_with(|| snapshots[left].2.cmp(&snapshots[right].2))
            }),
        }

        // Available accounts past a threshold. With `[server.pool]` set, the
        // soft-near ones (under the hard backstop) order by priority then
        // headroom — the all-near guard: a traffic spike degrades to
        // best-margin-first (within a priority tier) instead of emptying the
        // pool, mirroring the `available_under` tiebreak so a configured
        // primary stays preferred — and hard-over accounts still sort last.
        // Without it, soft == hard, so this is one rotation-order group
        // exactly as before #135.
        let mut near_soft = Vec::new();
        let mut over_hard = Vec::new();
        for &index in &rotation {
            if !is_available(index) || !snapshots[index].1.near {
                continue;
            }
            if pool.is_some() && !snapshots[index].1.over_hard {
                near_soft.push(index);
            } else {
                over_hard.push(index);
            }
        }
        near_soft.sort_by(|&left, &right| {
            accounts[left]
                .priority
                .cmp(&accounts[right].priority)
                .then_with(|| {
                    snapshots[right]
                        .1
                        .headroom
                        .total_cmp(&snapshots[left].1.headroom)
                })
        });

        let mut cooled = rotation
            .iter()
            .copied()
            .filter(|&index| snapshots[index].0.is_some_and(|until| until > now))
            .collect::<Vec<_>>();
        cooled.sort_by_key(|&index| snapshots[index].0);

        available_under
            .into_iter()
            .chain(near_soft)
            .chain(over_hard)
            .chain(cooled)
            .collect()
    }

    pub fn note_quota(&self, provider: &str, account: &AccountConfig, headers: &HeaderMap) {
        {
            let mut entries = self.entries.lock().expect("account health lock poisoned");
            let health = entries
                .entry((provider.to_string(), account_identity(account).to_string()))
                .or_default();
            health.observed = true;
            let quota = &mut health.quota;

            update_header(
                headers,
                "anthropic-ratelimit-unified-5h-utilization",
                &mut quota.utilization_5h,
            );
            update_header(
                headers,
                "anthropic-ratelimit-unified-5h-reset",
                &mut quota.reset_5h,
            );
            update_header(
                headers,
                "anthropic-ratelimit-unified-7d-utilization",
                &mut quota.utilization_7d,
            );
            update_header(
                headers,
                "anthropic-ratelimit-unified-7d-reset",
                &mut quota.reset_7d,
            );
            update_header(
                headers,
                "anthropic-ratelimit-unified-7d_oi-utilization",
                &mut quota.utilization_7d_oi,
            );
            update_header(
                headers,
                "anthropic-ratelimit-unified-7d_oi-reset",
                &mut quota.reset_7d_oi,
            );
            if let Some(status) = headers
                .get("anthropic-ratelimit-unified-status")
                .and_then(|value| value.to_str().ok())
            {
                quota.status = Some(status.to_string());
            }
            let utilization = pool_utilization(&mut entries, provider, unix_now());
            record_pool_utilization(provider, utilization);
        }
        self.mark_dirty();
    }

    /// Record the Codex backend's positional rate-limit header groups. A
    /// group's `window-minutes` identifies its bucket; the primary/secondary
    /// position does not. The recorded windows feed both the admin dashboard
    /// and Codex account selection via [`Self::select_order`] (issue #195).
    pub fn note_codex_quota(&self, provider: &str, account: &AccountConfig, headers: &HeaderMap) {
        {
            let mut entries = self.entries.lock().expect("account health lock poisoned");
            let health = entries
                .entry((provider.to_string(), account_identity(account).to_string()))
                .or_default();
            health.observed = true;
            let quota = &mut health.quota;

            for (minutes_header, utilization_header, reset_header) in [
                (
                    "x-codex-primary-window-minutes",
                    "x-codex-primary-used-percent",
                    "x-codex-primary-reset-at",
                ),
                (
                    "x-codex-secondary-window-minutes",
                    "x-codex-secondary-used-percent",
                    "x-codex-secondary-reset-at",
                ),
            ] {
                let minutes = header_value::<i64>(headers, minutes_header);
                let utilization = header_value::<f64>(headers, utilization_header)
                    .filter(|value| value.is_finite() && (0.0..=100.0).contains(value));
                let (Some(window), Some(utilization)) =
                    (minutes.and_then(codex_window_bucket), utilization)
                else {
                    continue;
                };
                let reset = header_value::<u64>(headers, reset_header);
                match window {
                    CodexWindow::FiveHour => {
                        quota.utilization_5h = Some(utilization / 100.0);
                        if let Some(reset) = reset {
                            quota.reset_5h = Some(reset);
                        }
                    }
                    CodexWindow::Weekly => {
                        quota.utilization_7d = Some(utilization / 100.0);
                        if let Some(reset) = reset {
                            quota.reset_7d = Some(reset);
                        }
                    }
                }
            }

            if let Some(status) = headers
                .get("x-codex-rate-limit-reached-type")
                .and_then(|value| value.to_str().ok())
            {
                quota.status = Some(status.to_string());
            }
            let utilization = pool_utilization(&mut entries, provider, unix_now());
            record_pool_utilization(provider, utilization);
        }
        self.mark_dirty();
    }

    /// Apply an authoritative usage snapshot from the Anthropic OAuth usage API
    /// to an account's quota state. Each reported window overwrites the matching
    /// utilization/reset pair — the usage API is authoritative and reconciles the
    /// header-derived state with out-of-band consumption — while a window the API
    /// omits leaves any prior header value untouched. The unified `status` is not
    /// modified here: the usage API has no equivalent of the header's `rejected`
    /// signal, so that stays header-driven. Marks the account observed, so the
    /// admin dashboard reports its usage even before the first proxied request.
    pub fn note_usage(&self, provider: &str, account: &AccountConfig, usage: &UsageSnapshot) {
        {
            let mut entries = self.entries.lock().expect("account health lock poisoned");
            let health = entries
                .entry((provider.to_string(), account_identity(account).to_string()))
                .or_default();
            health.observed = true;
            let quota = &mut health.quota;
            if let Some(window) = &usage.five_hour {
                quota.utilization_5h = Some(window.utilization);
                quota.reset_5h = window.resets_at;
            }
            if let Some(window) = &usage.seven_day {
                quota.utilization_7d = Some(window.utilization);
                quota.reset_7d = window.resets_at;
            }
            if let Some(window) = &usage.seven_day_oi {
                quota.utilization_7d_oi = Some(window.utilization);
                quota.reset_7d_oi = window.resets_at;
            }
            let utilization = pool_utilization(&mut entries, provider, unix_now());
            record_pool_utilization(provider, utilization);
        }
        self.mark_dirty();
    }

    pub fn cooldown(
        &self,
        provider: &str,
        account: &AccountConfig,
        duration: Duration,
        reason: &'static str,
    ) {
        let mut entries = self.entries.lock().expect("account health lock poisoned");
        let health = entries
            .entry((provider.to_string(), account_identity(account).to_string()))
            .or_default();
        health.observed = true;
        health.enabled = !account.disabled;
        health.cooldown_until = Some(Instant::now() + duration);
        // Any failover-worthy failure restarts the storm-control ramp: when the
        // account comes back it re-enters slow start instead of inheriting the
        // allowance it had grown before failing.
        health.ramp_allowance = 0;
        drop(entries);
        crate::metrics::record_pool_rotation(provider, reason);
    }

    /// Clear any cooldown and record the account as observed-healthy.
    /// `turn_succeeded` gates slow-start growth: a relayed client error (4xx)
    /// proves the account reachable — hence healthy — but must not pre-warm
    /// storm-control capacity, or a burst of malformed requests would bypass
    /// slow start before valid traffic arrives.
    pub fn mark_healthy(&self, provider: &str, account: &AccountConfig, turn_succeeded: bool) {
        let mut entries = self.entries.lock().expect("account health lock poisoned");
        let health = entries
            .entry((provider.to_string(), account_identity(account).to_string()))
            .or_default();
        health.observed = true;
        health.enabled = !account.disabled;
        health.cooldown_until = None;
        // Slow-start growth: each successful response doubles the identity's
        // admission allowance, so a healthy account leaves the ramp within a
        // handful of turns. `0` means the ramp is inactive (storm control off,
        // or a cooldown just reset it) — growing it here would skip the
        // re-seed in `try_admit`.
        if turn_succeeded && health.ramp_allowance > 0 {
            health.ramp_allowance = health.ramp_allowance.saturating_mul(2);
        }
    }

    /// Storm-control admission gate (issue #195): admit a request to this
    /// account identity only while its in-flight count is under the slow-start
    /// allowance. The allowance re-seeds to `initial` for an identity that has
    /// been idle for [`RAMP_IDLE_RESET`] (or whose ramp was reset by
    /// [`Self::cooldown`]), doubles per successful response
    /// ([`Self::mark_healthy`]), and is bypassed with `force` so a caller can
    /// always attempt its last remaining candidate rather than fail the
    /// request. Returns `None` when the identity is saturated; the returned
    /// guard releases the slot on drop. The guard is owned (holds the pool
    /// `Arc`) so callers can move it into the relayed response body stream —
    /// for a streaming turn the slot must stay held until the stream finishes,
    /// not just until upstream returns headers (the response body is lazy).
    pub fn try_admit(
        self: Arc<Self>,
        provider: &str,
        account: &AccountConfig,
        initial: u32,
        force: bool,
    ) -> Option<AdmissionGuard> {
        let key = (provider.to_string(), account_identity(account).to_string());
        {
            let mut entries = self.entries.lock().expect("account health lock poisoned");
            // Captured under the lock so `ramp_last_activity` (only ever
            // written under this same lock) can never be later than `now`.
            let now = Instant::now();
            let health = entries.entry(key.clone()).or_default();
            let idle = health.in_flight == 0
                && health
                    .ramp_last_activity
                    .is_none_or(|at| now.saturating_duration_since(at) >= RAMP_IDLE_RESET);
            if health.ramp_allowance == 0 || idle {
                health.ramp_allowance = initial.max(1);
            }
            if !force && health.in_flight >= health.ramp_allowance {
                tracing::debug!(
                    provider,
                    account = %account.name,
                    "storm control deferred admission; trying the next account"
                );
                return None;
            }
            health.in_flight = health.in_flight.saturating_add(1);
            health.ramp_last_activity = Some(now);
        }
        Some(AdmissionGuard { pool: self, key })
    }

    /// [`Self::try_admit`] applied to the candidate at `position` (0-based) of
    /// `candidates` in a failover order — the shared admission step of every
    /// pool loop. The outer `None` means the identity is saturated and the
    /// caller should rotate to the next candidate; `Some` carries the
    /// admission to hold — a guard, or `None` when admission gating is
    /// disabled (`ramp_initial` unset). The final candidate is always
    /// admitted (`force`): spilling a burst across the pool beats failing
    /// requests outright.
    pub fn admit_candidate(
        self: &Arc<Self>,
        provider: &str,
        account: &AccountConfig,
        ramp_initial: Option<u32>,
        position: usize,
        candidates: usize,
    ) -> Option<Option<AdmissionGuard>> {
        match ramp_initial {
            Some(initial) => {
                let force = position + 1 == candidates;
                self.clone()
                    .try_admit(provider, account, initial, force)
                    .map(Some)
            }
            None => Some(None),
        }
    }

    /// Forget pool health and refresh state for a single `(provider, identity)`,
    /// leaving other providers' entries for the same identity untouched. Removing
    /// persisted quota marks the pool dirty so the next flush removes it on disk.
    pub fn forget_identity(&self, provider: &str, identity: &str) {
        let key = (provider.to_string(), identity.to_string());
        let removed_quota = self
            .entries
            .lock()
            .expect("account health lock poisoned")
            .remove(&key)
            .is_some_and(|health| health.quota.has_signal());
        self.refresh_locks
            .lock()
            .expect("account refresh-lock map poisoned")
            .remove(&key);
        if removed_quota {
            self.mark_dirty();
        }
    }

    /// Read-only per-account health snapshot for the admin dashboard, in the
    /// given account order. Never mutates the round-robin cursor and never
    /// inserts entries for accounts the pool has not yet seen; it only clears
    /// quota buckets whose reset has already passed, exactly as the next
    /// `select_order` would. Carries no token material.
    pub fn snapshot(
        &self,
        provider: &str,
        accounts: &[AccountConfig],
        model: Option<&str>,
        pool: Option<&PoolConfig>,
    ) -> Vec<AccountSnapshot> {
        let now = Instant::now();
        let unix_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let is_fable = is_fable_model(model);
        let mut entries = self.entries.lock().expect("account health lock poisoned");
        accounts
            .iter()
            .map(|account| {
                let key = (provider.to_string(), account_identity(account).to_string());
                let Some(health) = entries.get_mut(&key).filter(|health| health.observed) else {
                    // Never selected, or selected but not yet answered (a default
                    // entry from `select_order`): report a clean, available slot.
                    return AccountSnapshot::unseen(account);
                };
                expire_stale_quota(&mut health.quota, unix_now);
                let quota = assess_quota(&health.quota, account, is_fable, pool, unix_now);
                let cooldown_secs_remaining = health
                    .cooldown_until
                    .and_then(|until| until.checked_duration_since(now))
                    .map(|remaining| remaining.as_secs());
                let cooling = cooldown_secs_remaining.is_some();
                AccountSnapshot {
                    name: account.name.clone(),
                    has_state: true,
                    available: !account.disabled && !cooling && !quota.near,
                    near_quota: quota.near,
                    cooldown_secs_remaining,
                    priority: account.priority,
                    disabled: account.disabled,
                    headroom_secs: (pool.is_some() && quota.headroom.is_finite())
                        .then_some(quota.headroom as i64),
                    utilization_5h: health.quota.utilization_5h,
                    reset_5h: health.quota.reset_5h,
                    utilization_7d: health.quota.utilization_7d,
                    reset_7d: health.quota.reset_7d,
                    utilization_7d_oi: health.quota.utilization_7d_oi,
                    reset_7d_oi: health.quota.reset_7d_oi,
                    status: health.quota.status.clone(),
                }
            })
            .collect()
    }

    /// Mark the pool's quota state as changed since the last flush. Called by
    /// every quota mutation so the opt-in persister ([`crate::state_persist`])
    /// can skip idle flushes. Also used to retry a failed persistence write.
    pub(crate) fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Atomically read-and-clear the dirty flag. Returns `true` when quota has
    /// changed since the previous call, meaning the persister should write.
    pub(crate) fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }

    /// Snapshot every observed account's quota for on-disk persistence, as
    /// `(provider, identity, quota)` triples. Skips accounts whose quota holds
    /// no utilization, reset, or status signal. Read-only: it does not expire
    /// stale windows (a restored quota is expired lazily by the next
    /// [`Self::select_order`]/[`Self::snapshot`], exactly as a live one is).
    pub fn export_quotas(&self) -> Vec<(String, String, QuotaState)> {
        let entries = self.entries.lock().expect("account health lock poisoned");
        entries
            .iter()
            .filter(|(_, health)| health.observed && health.quota.has_signal())
            .map(|((provider, identity), health)| {
                (provider.clone(), identity.clone(), health.quota.clone())
            })
            .collect()
    }

    /// Seed the pool with quotas restored from disk at boot. Each triple keys an
    /// account entry by `(provider, identity)`, marking it observed so the admin
    /// dashboard and `GET /usage` reflect the restored state before the first
    /// proxied request. Does not mark the pool dirty: freshly restored state
    /// need not be written straight back.
    pub fn import_quotas(&self, quotas: impl IntoIterator<Item = (String, String, QuotaState)>) {
        let mut entries = self.entries.lock().expect("account health lock poisoned");
        for (provider, identity, quota) in quotas {
            let health = entries.entry((provider, identity)).or_default();
            health.observed = true;
            health.quota = quota;
        }
    }

    /// Get the async mutex that serializes token refreshes for one account.
    ///
    /// The map's synchronous mutex is released before the returned lock can be
    /// awaited by the caller.
    pub fn refresh_lock(&self, provider: &str, account: &AccountConfig) -> Arc<AsyncMutex<()>> {
        let mut locks = self
            .refresh_locks
            .lock()
            .expect("account refresh-lock map poisoned");
        Arc::clone(
            locks
                .entry((provider.to_string(), account_identity(account).to_string()))
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        )
    }
}

/// RAII admission slot handed out by [`AccountPool::try_admit`]. Dropping it
/// releases the identity's in-flight slot and refreshes the idle-reset clock.
/// Hold it across the whole upstream attempt it admitted — for a relayed
/// streaming response that means moving it into the response body stream
/// (see `adapters::with_admission`), so the slot stays occupied until the
/// stream finishes or the client disconnects, not just until upstream
/// returned headers.
#[derive(Debug)]
pub struct AdmissionGuard {
    pool: Arc<AccountPool>,
    key: AccountKey,
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        let mut entries = self
            .pool
            .entries
            .lock()
            .expect("account health lock poisoned");
        if let Some(health) = entries.get_mut(&self.key) {
            health.in_flight = health.in_flight.saturating_sub(1);
            health.ramp_last_activity = Some(Instant::now());
        }
    }
}

/// Stable upstream identity used for pool health and candidate coalescing.
/// Claude stores `shuntAccountUuid` and Codex stores `chatgpt_account_id` in
/// [`AccountConfig::uuid`]; accounts without either remain distinct by name.
/// A blank (empty or all-whitespace) `uuid` is treated the same as a missing
/// one — otherwise every account configured with `uuid = ""` would coalesce
/// into a single shared identity instead of falling back to its own name.
pub(crate) fn account_identity(account: &AccountConfig) -> &str {
    match account.uuid.as_deref() {
        Some(uuid) if !uuid.trim().is_empty() => uuid,
        _ => &account.name,
    }
}

/// Collapse accounts sharing a stable upstream identity ([`account_identity`])
/// down to one representative per identity, keeping the enabled (or, among
/// equally-disabled duplicates, the lowest-priority) account as the
/// representative. Collision *warnings* are not emitted here: this runs on
/// every [`AccountPool::select_order`] call (the request hot path), so
/// logging here would re-warn per request. Configured-account collisions are
/// caught once at config load (`crate::config::identity_collisions`);
/// store-discovered collisions are caught once per store scan (see
/// `crate::auth::shared::scan_cached`), not here.
fn collapse_representatives(accounts: &[AccountConfig]) -> Vec<usize> {
    let mut slots = HashMap::<&str, usize>::with_capacity(accounts.len());
    let mut representatives: Vec<usize> = Vec::with_capacity(accounts.len());
    for (index, account) in accounts.iter().enumerate() {
        let identity = account_identity(account);
        if let Some(&slot) = slots.get(identity) {
            let current = &accounts[representatives[slot]];
            if (current.disabled && !account.disabled)
                || (current.disabled == account.disabled && account.priority < current.priority)
            {
                representatives[slot] = index;
            }
        } else {
            slots.insert(identity, representatives.len());
            representatives.push(index);
        }
    }
    representatives
}

fn stable_session_index(session_id: &str, account_count: usize) -> usize {
    let digest = Sha256::digest(session_id.as_bytes());
    let prefix = u64::from_be_bytes(digest[..8].try_into().expect("SHA-256 prefix is 8 bytes"));
    (prefix % account_count as u64) as usize
}

fn header_value<T: std::str::FromStr>(headers: &HeaderMap, name: &str) -> Option<T> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<T>().ok())
}

fn update_header<T: std::str::FromStr>(headers: &HeaderMap, name: &str, field: &mut Option<T>) {
    if let Some(parsed) = header_value(headers, name) {
        *field = Some(parsed);
    }
}

fn codex_window_bucket(minutes: i64) -> Option<CodexWindow> {
    if within_five_percent(minutes, 300) {
        Some(CodexWindow::FiveHour)
    } else if within_five_percent(minutes, 10_080) {
        Some(CodexWindow::Weekly)
    } else {
        None
    }
}

fn within_five_percent(value: i64, expected: i64) -> bool {
    let Some(scaled) = value.checked_mul(100) else {
        return false;
    };
    scaled >= expected * 95 && scaled <= expected * 105
}

fn is_fable_model(model: Option<&str>) -> bool {
    model.is_some_and(|model| model.to_ascii_lowercase().contains("fable"))
}

fn governing_weekly_reset(quota: &QuotaState, is_fable: bool) -> Option<u64> {
    if is_fable && quota.utilization_7d_oi.is_some() {
        quota.reset_7d_oi
    } else {
        quota.reset_7d
    }
}

/// Resolve the soft threshold for one quota window:
/// account `threshold_X` → account `threshold` → pool `default_threshold_X` →
/// pool `default_threshold` → hard threshold. The hard backstop caps the
/// result so a soft threshold can never exceed it.
fn resolved_threshold(
    window: QuotaWindow,
    account: &AccountConfig,
    pool: Option<&PoolConfig>,
) -> f64 {
    let hard = pool.map_or(SWITCH_THRESHOLD, |pool| pool.hard_threshold);
    let account_window = match window {
        QuotaWindow::FiveHour => account.threshold_5h,
        QuotaWindow::Weekly => account.threshold_7d,
        QuotaWindow::Fable => account.threshold_fable,
    };
    let pool_default = pool.and_then(|pool| {
        let per_window = match window {
            QuotaWindow::FiveHour => pool.default_threshold_5h,
            QuotaWindow::Weekly => pool.default_threshold_7d,
            QuotaWindow::Fable => pool.default_threshold_fable,
        };
        per_window.or(pool.default_threshold)
    });
    account_window
        .or(account.threshold)
        .or(pool_default)
        .unwrap_or(hard)
        .min(hard)
}

/// Per-account quota verdict across the windows that govern the request's
/// model: the 5h window always, plus the fable `7d_oi` bucket when the model
/// is fable and that bucket has been observed, otherwise the shared `7d`
/// bucket (the same governing choice as [`governing_weekly_reset`]).
#[derive(Debug, Clone)]
struct QuotaAssessment {
    /// Past a soft threshold, upstream-rejected, or (with burn-rate avoidance
    /// on) projected to exhaust a window before it resets.
    near: bool,
    /// Past the hard backstop; always sorts last among available accounts.
    over_hard: bool,
    /// Minimum burn-rate headroom in seconds across the governing windows
    /// (see [`window_headroom`]); +∞ when nothing suggests pressure.
    headroom: f64,
}

fn assess_quota(
    quota: &QuotaState,
    account: &AccountConfig,
    is_fable: bool,
    pool: Option<&PoolConfig>,
    now: u64,
) -> QuotaAssessment {
    let hard = pool.map_or(SWITCH_THRESHOLD, |pool| pool.hard_threshold);
    let burn_avoid = pool.is_some_and(|pool| pool.burn_rate_avoidance);
    let rejected = quota.status.as_deref() == Some("rejected");
    let mut assessment = QuotaAssessment {
        near: rejected,
        over_hard: false,
        // An upstream rejection is zero headroom by definition, whatever the
        // utilization numbers said.
        headroom: if rejected {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        },
    };

    let weekly = if is_fable && quota.utilization_7d_oi.is_some() {
        (
            quota.utilization_7d_oi,
            quota.reset_7d_oi,
            QuotaWindow::Fable,
        )
    } else {
        (quota.utilization_7d, quota.reset_7d, QuotaWindow::Weekly)
    };
    let windows = [
        (
            quota.utilization_5h,
            quota.reset_5h,
            WINDOW_5H_SECS,
            QuotaWindow::FiveHour,
        ),
        (weekly.0, weekly.1, WINDOW_7D_SECS, weekly.2),
    ];
    for (utilization, reset, window_len, window) in windows {
        let Some(utilization) = utilization else {
            continue;
        };
        let threshold = resolved_threshold(window, account, pool);
        if utilization >= threshold {
            assessment.near = true;
        }
        if utilization >= hard {
            assessment.over_hard = true;
        }
        let headroom = window_headroom(utilization, reset, window_len, threshold, now);
        if burn_avoid && headroom < 0.0 {
            assessment.near = true;
        }
        assessment.headroom = assessment.headroom.min(headroom);
    }
    assessment
}

/// Projected margin, in seconds, for one quota window: the time until
/// utilization reaches the soft threshold at the observed average burn speed,
/// minus the time until the window resets. Positive means the account
/// survives to its reset at the current pace; negative means it is burning
/// too fast. Missing data means "no evidence of pressure" (+∞), so
/// unobserved accounts keep sorting first, and a window already at or past
/// its threshold is −∞.
fn window_headroom(
    utilization: f64,
    reset: Option<u64>,
    window_len: u64,
    threshold: f64,
    now: u64,
) -> f64 {
    let budget_left = threshold - utilization;
    if budget_left <= 0.0 {
        return f64::NEG_INFINITY;
    }
    if utilization <= 0.0 {
        return f64::INFINITY;
    }
    let Some(reset) = reset else {
        return f64::INFINITY;
    };
    // The headers carry only the reset instant, so the window start is derived
    // from the hardcoded window length; elapsed is clamped away from zero so a
    // window that just opened never divides by zero. `now` is clamped into
    // [window_start, reset] first so a desynced local clock cannot push elapsed
    // or time_to_reset outside the physically valid [0, window_len] range.
    let window_start = reset.saturating_sub(window_len);
    let now_clamped = now.clamp(window_start, reset);
    let elapsed = now_clamped
        .saturating_sub(window_start)
        .clamp(1, window_len) as f64;
    let burn_speed = utilization / elapsed;
    let time_to_exhaust = budget_left / burn_speed;
    let time_to_reset = reset.saturating_sub(now_clamped) as f64;
    time_to_exhaust - time_to_reset
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Best-account utilization for each non-stale quota window. Disabled entries,
/// unobserved values, non-finite values, and expired windows are excluded.
fn pool_utilization(
    entries: &mut HashMap<AccountKey, AccountHealth>,
    provider: &str,
    now: u64,
) -> [Option<f64>; 3] {
    let mut minimums = [None::<f64>; 3];
    for ((entry_provider, _), health) in entries.iter_mut() {
        if entry_provider != provider || !health.enabled {
            continue;
        }
        expire_stale_quota(&mut health.quota, now);
        for (minimum, value) in minimums.iter_mut().zip([
            health.quota.utilization_5h,
            health.quota.utilization_7d,
            health.quota.utilization_7d_oi,
        ]) {
            let Some(value) = value.filter(|value| value.is_finite()) else {
                continue;
            };
            let value = value.clamp(0.0, 1.0);
            *minimum = Some(minimum.map_or(value, |current| current.min(value)));
        }
    }
    minimums
}

fn record_pool_utilization(provider: &str, utilization: [Option<f64>; 3]) {
    for (window, value) in ["5h", "7d", "7d_oi"].into_iter().zip(utilization) {
        crate::metrics::record_pool_utilization(provider, window, value);
    }
}

fn expire_stale_quota(quota: &mut QuotaState, now: u64) {
    let mut expired = false;
    if quota.reset_5h.is_some_and(|reset| reset <= now) {
        quota.utilization_5h = None;
        quota.reset_5h = None;
        expired = true;
    }
    if quota.reset_7d.is_some_and(|reset| reset <= now) {
        quota.utilization_7d = None;
        quota.reset_7d = None;
        expired = true;
    }
    if quota.reset_7d_oi.is_some_and(|reset| reset <= now) {
        quota.utilization_7d_oi = None;
        quota.reset_7d_oi = None;
        expired = true;
    }
    if expired {
        quota.status = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailoverAction {
    Relay,
    Rotate,
    PauseSame,
    RefreshRetry,
}

const QUOTA_STATUS_HEADERS: [&str; 3] = [
    "anthropic-ratelimit-unified-5h-status",
    "anthropic-ratelimit-unified-7d-status",
    "anthropic-ratelimit-unified-7d_oi-status",
];

/// Low-cardinality pool-rotation reason for an upstream response that moves off
/// an account. A quota-rejected Anthropic 429 is distinguished from ordinary
/// throttling; 5xx and 401 retain their operational categories.
pub fn rotation_reason(status: StatusCode, headers: &HeaderMap) -> &'static str {
    if status == StatusCode::TOO_MANY_REQUESTS {
        if QUOTA_STATUS_HEADERS
            .iter()
            .any(|name| headers.get(*name).is_some_and(|value| value == "rejected"))
        {
            "quota"
        } else {
            "rate_limit"
        }
    } else if status == StatusCode::UNAUTHORIZED {
        "auth"
    } else if status.is_server_error() {
        "server_error"
    } else {
        "other"
    }
}

pub fn classify(status: StatusCode, headers: &HeaderMap) -> FailoverAction {
    if status.is_success() {
        return FailoverAction::Relay;
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        if QUOTA_STATUS_HEADERS
            .iter()
            .any(|name| headers.get(*name).is_some_and(|value| value == "rejected"))
        {
            return FailoverAction::Rotate;
        }
        return FailoverAction::PauseSame;
    }
    if status == StatusCode::UNAUTHORIZED {
        return FailoverAction::RefreshRetry;
    }
    if status.is_server_error() {
        return FailoverAction::Rotate;
    }
    FailoverAction::Relay
}

/// Classify a Codex/ChatGPT upstream response for account-pool failover.
/// Takes the same `(status, headers)` shape as [`classify`] so both adapters
/// share one call site. Codex quota/rejection headers are display-only: every
/// 429 still rotates rather than pausing the same account.
pub fn classify_codex(status: StatusCode, _headers: &HeaderMap) -> FailoverAction {
    if status.is_success() {
        return FailoverAction::Relay;
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return FailoverAction::Rotate;
    }
    if status == StatusCode::UNAUTHORIZED {
        return FailoverAction::RefreshRetry;
    }
    if status.is_server_error() {
        return FailoverAction::Rotate;
    }
    FailoverAction::Relay
}

pub fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    // RFC 7231 allows two forms: delta-seconds or an HTTP-date. Try the cheap
    // numeric form first, then fall back to the date form — a server that sends
    // `Retry-After: <HTTP-date>` would otherwise be silently ignored.
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let deadline = httpdate::parse_http_date(value.trim()).ok()?;
    // Honor the wait until that instant; a deadline already in the past means
    // "retry now" (zero wait) rather than falling through to computed backoff.
    Some(
        deadline
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    use super::*;

    fn account(name: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_string(),
            ..Default::default()
        }
    }

    fn account_with_uuid(name: &str, uuid: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_string(),
            uuid: Some(uuid.to_string()),
            ..Default::default()
        }
    }

    fn accounts() -> Vec<AccountConfig> {
        ["a", "b", "c", "d"].into_iter().map(account).collect()
    }

    fn quota_headers(values: &[(&'static str, String)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in values {
            headers.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    #[test]
    fn shared_identity_is_enabled_when_any_alias_is_enabled() {
        let pool = AccountPool::new();
        let accounts = vec![
            AccountConfig {
                name: "enabled".to_string(),
                uuid: Some("shared".to_string()),
                ..Default::default()
            },
            AccountConfig {
                name: "disabled".to_string(),
                uuid: Some("shared".to_string()),
                disabled: true,
                ..Default::default()
            },
        ];
        pool.select_order("anthropic", &accounts, Some("session"), None, None);

        let entries = pool.entries.lock().expect("account health lock poisoned");
        assert!(entries
            .get(&("anthropic".to_string(), "shared".to_string()))
            .is_some_and(|health| health.enabled));
    }

    #[test]
    fn syncing_enabled_accounts_preserves_aliases_and_disables_removed_identities() {
        let pool = AccountPool::new();
        let initial = vec![
            account_with_uuid("enabled", "shared"),
            account_with_uuid("removed", "removed-id"),
        ];
        pool.sync_enabled_accounts("anthropic", &initial);

        let current = vec![
            AccountConfig {
                name: "disabled-alias".to_string(),
                uuid: Some("shared".to_string()),
                disabled: true,
                ..Default::default()
            },
            account_with_uuid("enabled-alias", "shared"),
        ];
        pool.sync_enabled_accounts("anthropic", &current);

        let entries = pool.entries.lock().expect("account health lock poisoned");
        assert!(entries
            .get(&("anthropic".to_string(), "shared".to_string()))
            .is_some_and(|health| health.enabled));
        assert!(entries
            .get(&("anthropic".to_string(), "removed-id".to_string()))
            .is_some_and(|health| !health.enabled));
    }

    #[test]
    fn quota_update_does_not_override_synchronized_alias_state() {
        let pool = AccountPool::new();
        let accounts = vec![
            account_with_uuid("enabled", "shared"),
            AccountConfig {
                name: "disabled".to_string(),
                uuid: Some("shared".to_string()),
                disabled: true,
                ..Default::default()
            },
        ];
        pool.sync_enabled_accounts("anthropic", &accounts);
        pool.note_quota("anthropic", &accounts[1], &HeaderMap::new());

        let entries = pool.entries.lock().expect("account health lock poisoned");
        assert!(entries
            .get(&("anthropic".to_string(), "shared".to_string()))
            .is_some_and(|health| health.enabled));
    }

    #[test]
    fn pool_utilization_uses_best_enabled_non_stale_account() {
        let now = unix_now();
        let mut entries = HashMap::from([
            (
                ("anthropic".to_string(), "best".to_string()),
                AccountHealth {
                    quota: QuotaState {
                        utilization_5h: Some(0.2),
                        reset_5h: Some(now + 3600),
                        utilization_7d: Some(0.6),
                        reset_7d: Some(now + 3600),
                        ..Default::default()
                    },
                    enabled: true,
                    observed: true,
                    ..Default::default()
                },
            ),
            (
                ("anthropic".to_string(), "other".to_string()),
                AccountHealth {
                    quota: QuotaState {
                        utilization_5h: Some(0.7),
                        reset_5h: Some(now + 3600),
                        utilization_7d_oi: Some(0.4),
                        reset_7d_oi: Some(now + 3600),
                        ..Default::default()
                    },
                    enabled: true,
                    observed: true,
                    ..Default::default()
                },
            ),
            (
                ("anthropic".to_string(), "disabled".to_string()),
                AccountHealth {
                    quota: QuotaState {
                        utilization_5h: Some(0.01),
                        reset_5h: Some(now + 3600),
                        ..Default::default()
                    },
                    enabled: false,
                    observed: true,
                    ..Default::default()
                },
            ),
            (
                ("anthropic".to_string(), "stale".to_string()),
                AccountHealth {
                    quota: QuotaState {
                        utilization_5h: Some(0.05),
                        reset_5h: Some(now.saturating_sub(1)),
                        ..Default::default()
                    },
                    enabled: true,
                    observed: true,
                    ..Default::default()
                },
            ),
        ]);

        assert_eq!(
            pool_utilization(&mut entries, "anthropic", now),
            [Some(0.2), Some(0.6), Some(0.4)]
        );
    }

    #[test]
    fn rotation_reason_is_low_cardinality_and_distinguishes_quota() {
        let mut quota = HeaderMap::new();
        quota.insert(
            "anthropic-ratelimit-unified-5h-status",
            HeaderValue::from_static("rejected"),
        );
        assert_eq!(
            rotation_reason(StatusCode::TOO_MANY_REQUESTS, &quota),
            "quota"
        );
        assert_eq!(
            rotation_reason(StatusCode::TOO_MANY_REQUESTS, &HeaderMap::new()),
            "rate_limit"
        );
        assert_eq!(
            rotation_reason(StatusCode::UNAUTHORIZED, &HeaderMap::new()),
            "auth"
        );
        assert_eq!(
            rotation_reason(StatusCode::BAD_GATEWAY, &HeaderMap::new()),
            "server_error"
        );
    }

    #[test]
    fn session_selection_is_stable_and_spreads_across_sessions() {
        let pool = AccountPool::new();
        let accounts = accounts();
        let first = pool.select_order("anthropic", &accounts, Some("session-a"), None, None);
        assert_eq!(
            first,
            pool.select_order("anthropic", &accounts, Some("session-a"), None, None)
        );
        assert_eq!(first[0], stable_session_index("session-a", accounts.len()));

        let starts = (0..64)
            .map(|id| {
                pool.select_order(
                    "anthropic",
                    &accounts,
                    Some(&format!("session-{id}")),
                    None,
                    None,
                )[0]
            })
            .collect::<HashSet<_>>();
        assert!(starts.len() > 1);
    }

    #[test]
    fn blank_uuid_falls_back_to_name_instead_of_coalescing() {
        // uuid = "" (or all-whitespace) must not coalesce distinct accounts the
        // way a real shared uuid does — it is treated as absent, like `None`.
        let empty_a = account_with_uuid("empty-a", "");
        let empty_b = account_with_uuid("empty-b", "   ");
        assert_eq!(account_identity(&empty_a), "empty-a");
        assert_eq!(account_identity(&empty_b), "empty-b");

        let pool = AccountPool::new();
        let accounts = vec![empty_a, empty_b];
        let order = pool.select_order("anthropic", &accounts, Some("session"), None, None);
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn same_identity_is_one_selection_candidate() {
        let pool = AccountPool::new();
        let accounts = vec![
            account_with_uuid("alias-a", "shared"),
            account_with_uuid("alias-b", "shared"),
            account_with_uuid("other", "other"),
        ];

        let order = pool.select_order("anthropic", &accounts, Some("session"), None, None);
        assert_eq!(order.len(), 2);
        assert_eq!(
            order
                .iter()
                .map(|&index| account_identity(&accounts[index]))
                .collect::<HashSet<_>>()
                .len(),
            2
        );
        assert_eq!(order.iter().filter(|&&index| index < 2).count(), 1);
    }

    #[test]
    fn shared_identity_cooldown_applies_to_aliases_and_sorts_last() {
        let pool = AccountPool::new();
        let accounts = vec![
            account_with_uuid("alias-a", "shared"),
            account_with_uuid("alias-b", "shared"),
            account_with_uuid("other", "other"),
        ];
        pool.cooldown(
            "anthropic",
            &accounts[0],
            Duration::from_secs(60),
            "transport",
        );

        let snapshots = pool.snapshot("anthropic", &accounts, None, None);
        for snapshot in &snapshots[..2] {
            assert!(snapshot.has_state);
            assert!(!snapshot.available);
            assert!(snapshot.cooldown_secs_remaining.is_some());
        }
        let order = pool.select_order("anthropic", &accounts, Some("session"), None, None);
        assert_eq!(account_identity(&accounts[order[0]]), "other");
        assert_eq!(
            account_identity(&accounts[*order.last().unwrap()]),
            "shared"
        );
    }

    #[test]
    fn shared_identity_quota_is_visible_on_every_alias() {
        let pool = AccountPool::new();
        let accounts = vec![
            account_with_uuid("alias-a", "shared"),
            account_with_uuid("alias-b", "shared"),
        ];
        pool.note_quota(
            "anthropic",
            &accounts[0],
            &quota_headers(&[(
                "anthropic-ratelimit-unified-5h-utilization",
                "0.99".to_string(),
            )]),
        );

        let snapshots = pool.snapshot("anthropic", &accounts, None, None);
        assert!(snapshots.iter().all(|snapshot| snapshot.near_quota));
        assert!(snapshots
            .iter()
            .all(|snapshot| snapshot.utilization_5h == Some(0.99)));
    }

    #[test]
    fn alias_changes_do_not_move_a_sticky_identity() {
        let pool = AccountPool::new();
        let base = vec![
            account_with_uuid("primary", "shared"),
            account_with_uuid("other", "other"),
        ];
        let expanded = vec![
            account_with_uuid("primary", "shared"),
            account_with_uuid("primary-alias", "shared"),
            account_with_uuid("other", "other"),
        ];

        for session in ["sticky-a", "sticky-b", "sticky-c"] {
            let base_order = pool.select_order("anthropic", &base, Some(session), None, None);
            let expanded_order =
                pool.select_order("anthropic", &expanded, Some(session), None, None);
            assert_eq!(
                account_identity(&base[base_order[0]]),
                account_identity(&expanded[expanded_order[0]])
            );
        }
    }

    #[test]
    fn accounts_without_uuid_remain_distinct() {
        let pool = AccountPool::new();
        let accounts = vec![account("a"), account("b")];
        let order = pool.select_order("anthropic", &accounts, Some("session"), None, None);
        assert_eq!(order.len(), accounts.len());
        assert_eq!(order.iter().copied().collect::<HashSet<_>>().len(), 2);
    }

    #[test]
    fn representative_prefers_enabled_then_priority_then_first_seen() {
        let mut disabled = account_with_uuid("disabled", "shared");
        disabled.disabled = true;
        disabled.priority = 1;
        let mut preferred = account_with_uuid("preferred", "shared");
        preferred.priority = 10;
        let mut later = account_with_uuid("later", "shared");
        later.priority = 10;
        let other = account_with_uuid("other", "other");
        let accounts = vec![disabled, preferred, later, other];

        assert_eq!(collapse_representatives(&accounts), vec![1, 3]);

        let mut all_disabled = accounts;
        for account in &mut all_disabled[..3] {
            account.disabled = true;
        }
        let pool = AccountPool::new();
        let order = pool.select_order("anthropic", &all_disabled, Some("session"), None, None);
        assert_eq!(order, vec![3]);
    }

    #[test]
    fn round_robin_advances_over_distinct_identities() {
        let pool = AccountPool::new();
        let accounts = vec![
            account_with_uuid("alias-a", "shared"),
            account_with_uuid("alias-b", "shared"),
            account_with_uuid("other", "other"),
        ];

        let starts = (0..3)
            .map(|_| {
                let order = pool.select_order("anthropic", &accounts, None, None, None);
                account_identity(&accounts[order[0]])
            })
            .collect::<Vec<_>>();
        assert_eq!(starts, vec!["shared", "other", "shared"]);
    }

    #[test]
    fn refresh_locks_are_shared_by_identity() {
        let pool = AccountPool::new();
        let first = account_with_uuid("alias-a", "shared");
        let second = account_with_uuid("alias-b", "shared");
        assert!(Arc::ptr_eq(
            &pool.refresh_lock("anthropic", &first),
            &pool.refresh_lock("anthropic", &second)
        ));
    }

    #[test]
    fn healthy_under_threshold_sticky_account_stays_first() {
        let pool = AccountPool::new();
        let accounts = accounts();
        let session = "healthy-sticky";
        let first = pool.select_order("anthropic", &accounts, Some(session), None, None);
        let sticky = first[0];
        pool.note_quota(
            "anthropic",
            &accounts[sticky],
            &quota_headers(&[(
                "anthropic-ratelimit-unified-5h-utilization",
                "0.97".to_string(),
            )]),
        );
        assert_eq!(
            pool.select_order("anthropic", &accounts, Some(session), None, None),
            first
        );
    }

    #[test]
    fn near_quota_sticky_rotates_to_fresh_account() {
        let pool = AccountPool::new();
        let accounts = accounts();
        let session = "quota-sticky";
        let original = pool.select_order("anthropic", &accounts, Some(session), None, None);
        let sticky = original[0];
        pool.note_quota(
            "anthropic",
            &accounts[sticky],
            &quota_headers(&[(
                "anthropic-ratelimit-unified-5h-utilization",
                "0.98".to_string(),
            )]),
        );
        let rotated = pool.select_order("anthropic", &accounts, Some(session), None, None);
        assert_ne!(rotated[0], sticky);
        assert_eq!(rotated.last(), Some(&sticky));
    }

    #[test]
    fn snapshot_reports_health_for_seen_accounts() {
        let pool = AccountPool::new();
        let accounts = vec![
            account("seen-near"),
            account("seen-cool"),
            account("unseen"),
        ];

        // One account near its 5h quota, one on cooldown; the third is never
        // touched, so it must report as an unseen, available slot.
        pool.note_quota(
            "anthropic",
            &accounts[0],
            &quota_headers(&[(
                "anthropic-ratelimit-unified-5h-utilization",
                "0.99".to_string(),
            )]),
        );
        pool.cooldown(
            "anthropic",
            &accounts[1],
            Duration::from_secs(45),
            "transport",
        );

        let snaps = pool.snapshot("anthropic", &accounts, None, None);
        assert_eq!(snaps.len(), 3);

        let near = &snaps[0];
        assert!(near.has_state);
        assert!(near.near_quota);
        assert!(!near.available, "a near-quota account is not available");
        assert!(near.utilization_5h.unwrap() > 0.98);

        let cool = &snaps[1];
        assert!(cool.has_state);
        assert!(!cool.available, "a cooling account is not available");
        assert!(cool.cooldown_secs_remaining.unwrap() > 0);

        let unseen = &snaps[2];
        assert!(!unseen.has_state);
        assert!(unseen.available);
        assert!(unseen.cooldown_secs_remaining.is_none());
    }

    #[test]
    fn codex_weekly_header_group_maps_by_window_minutes() {
        let pool = AccountPool::new();
        let accounts = vec![account("pro")];
        let reset = unix_now() + 508_740;
        let headers = quota_headers(&[
            ("x-codex-primary-used-percent", "26".to_string()),
            ("x-codex-primary-window-minutes", "10080".to_string()),
            ("x-codex-primary-reset-at", reset.to_string()),
            ("x-codex-primary-reset-after-seconds", "508740".to_string()),
            ("x-codex-secondary-used-percent", "0".to_string()),
            ("x-codex-secondary-window-minutes", "0".to_string()),
            ("x-codex-secondary-reset-at", String::new()),
            ("x-codex-plan-type", "pro".to_string()),
            ("x-codex-active-limit", "premium".to_string()),
        ]);

        pool.note_codex_quota("codex", &accounts[0], &headers);

        let snaps = pool.snapshot("codex", &accounts, None, None);
        assert!(snaps[0].has_state);
        assert_eq!(snaps[0].utilization_7d, Some(0.26));
        assert_eq!(snaps[0].reset_7d, Some(reset));
        assert_eq!(snaps[0].utilization_5h, None);
        assert_eq!(snaps[0].utilization_7d_oi, None);
    }

    #[test]
    fn codex_quota_uses_stable_uuid_identity() {
        let pool = AccountPool::new();
        let accounts = vec![account_with_uuid("pro", "account-uuid")];
        let headers = quota_headers(&[
            ("x-codex-primary-used-percent", "40".to_string()),
            ("x-codex-primary-window-minutes", "300".to_string()),
        ]);

        pool.note_codex_quota("codex", &accounts[0], &headers);

        let snaps = pool.snapshot("codex", &accounts, None, None);
        assert!(snaps[0].has_state);
        assert_eq!(snaps[0].utilization_5h, Some(0.4));
    }

    #[test]
    fn codex_five_hour_header_group_maps_by_window_minutes() {
        let pool = AccountPool::new();
        let accounts = vec![account("pro")];
        let headers = quota_headers(&[
            ("x-codex-primary-used-percent", "40".to_string()),
            ("x-codex-primary-window-minutes", "300".to_string()),
        ]);

        pool.note_codex_quota("codex", &accounts[0], &headers);

        let snaps = pool.snapshot("codex", &accounts, None, None);
        assert!(snaps[0].has_state);
        assert_eq!(snaps[0].utilization_5h, Some(0.4));
        assert_eq!(snaps[0].utilization_7d, None);
    }

    #[test]
    fn codex_unmatched_window_is_ignored() {
        let pool = AccountPool::new();
        let accounts = vec![account("pro")];
        let headers = quota_headers(&[
            ("x-codex-primary-used-percent", "75.0".to_string()),
            ("x-codex-primary-window-minutes", "1440".to_string()),
        ]);

        pool.note_codex_quota("codex", &accounts[0], &headers);

        let snaps = pool.snapshot("codex", &accounts, None, None);
        assert!(snaps[0].has_state);
        assert_eq!(snaps[0].utilization_5h, None);
        assert_eq!(snaps[0].utilization_7d, None);
    }

    #[test]
    fn codex_missing_reset_preserves_prior_reset() {
        let pool = AccountPool::new();
        let accounts = vec![account("pro")];
        let reset = unix_now() + 3_600;
        pool.note_codex_quota(
            "codex",
            &accounts[0],
            &quota_headers(&[
                ("x-codex-primary-used-percent", "40".to_string()),
                ("x-codex-primary-window-minutes", "300".to_string()),
                ("x-codex-primary-reset-at", reset.to_string()),
            ]),
        );

        pool.note_codex_quota(
            "codex",
            &accounts[0],
            &quota_headers(&[
                ("x-codex-primary-used-percent", "41".to_string()),
                ("x-codex-primary-window-minutes", "300".to_string()),
            ]),
        );

        let snaps = pool.snapshot("codex", &accounts, None, None);
        assert_eq!(snaps[0].utilization_5h, Some(0.41));
        assert_eq!(snaps[0].reset_5h, Some(reset));
    }

    #[test]
    fn codex_invalid_utilization_is_ignored() {
        for utilization in ["NaN", "-1", "101"] {
            let pool = AccountPool::new();
            let accounts = vec![account("pro")];
            let headers = quota_headers(&[
                ("x-codex-primary-used-percent", utilization.to_string()),
                ("x-codex-primary-window-minutes", "300".to_string()),
            ]);

            pool.note_codex_quota("codex", &accounts[0], &headers);

            let snaps = pool.snapshot("codex", &accounts, None, None);
            assert!(snaps[0].has_state);
            assert_eq!(snaps[0].utilization_5h, None);
        }
    }

    #[test]
    fn codex_rejection_status_is_recorded_for_display_only() {
        let pool = AccountPool::new();
        let accounts = vec![account("pro")];
        pool.note_codex_quota(
            "codex",
            &accounts[0],
            &quota_headers(&[("x-codex-rate-limit-reached-type", "weekly".to_string())]),
        );

        let snaps = pool.snapshot("codex", &accounts, None, None);
        assert_eq!(snaps[0].status.as_deref(), Some("weekly"));
    }

    #[test]
    fn codex_quota_rotates_off_near_quota_sticky_account() {
        // Issue #195: the recorded x-codex-* windows are no longer display-only —
        // an exhausted sticky account proactively yields to the other account
        // even without [server.pool] tuning (legacy 0.98 hard threshold).
        let pool = AccountPool::new();
        let accounts = vec![account("a"), account("b")];
        let session = "codex-quota-aware";
        let initial = pool.select_order("codex", &accounts, Some(session), None, None);
        let sticky = initial[0];
        pool.note_codex_quota(
            "codex",
            &accounts[sticky],
            &quota_headers(&[
                ("x-codex-primary-used-percent", "100".to_string()),
                ("x-codex-primary-window-minutes", "300".to_string()),
            ]),
        );

        let order = pool.select_order("codex", &accounts, Some(session), None, None);
        assert_eq!(order.len(), 2);
        assert_ne!(order[0], sticky, "exhausted sticky account must yield");
        assert_eq!(order[1], sticky, "near-quota account stays as fallback");
    }

    #[test]
    fn codex_quota_under_threshold_keeps_sticky_account() {
        let pool = AccountPool::new();
        let accounts = vec![account("a"), account("b")];
        let session = "codex-quota-under";
        let initial = pool.select_order("codex", &accounts, Some(session), None, None);
        let sticky = initial[0];
        pool.note_codex_quota(
            "codex",
            &accounts[sticky],
            &quota_headers(&[
                ("x-codex-primary-used-percent", "50".to_string()),
                ("x-codex-primary-window-minutes", "300".to_string()),
            ]),
        );

        assert_eq!(
            pool.select_order("codex", &accounts, Some(session), None, None),
            initial
        );
    }

    #[test]
    fn try_admit_caps_concurrency_and_force_bypasses() {
        let pool = Arc::new(AccountPool::new());
        let acc = account("a");
        let first = pool
            .clone()
            .try_admit("codex", &acc, 2, false)
            .expect("first admission fits the initial allowance");
        let second = pool
            .clone()
            .try_admit("codex", &acc, 2, false)
            .expect("second admission fits the initial allowance");
        assert!(
            pool.clone().try_admit("codex", &acc, 2, false).is_none(),
            "a saturated identity defers further admissions"
        );
        let forced = pool
            .clone()
            .try_admit("codex", &acc, 2, true)
            .expect("force admits past the allowance for the last candidate");
        drop((first, second, forced));
    }

    #[test]
    fn admit_candidate_rotates_when_saturated_and_forces_the_last() {
        let pool = Arc::new(AccountPool::new());
        let acc = account("a");
        assert!(
            matches!(pool.admit_candidate("codex", &acc, None, 0, 2), Some(None)),
            "disabled gating admits without a guard"
        );
        let first = pool
            .admit_candidate("codex", &acc, Some(1), 0, 2)
            .expect("first admission fits the initial allowance")
            .expect("enabled gating returns a guard");
        assert!(
            pool.admit_candidate("codex", &acc, Some(1), 0, 2).is_none(),
            "a saturated identity rotates a non-final candidate"
        );
        let forced = pool
            .admit_candidate("codex", &acc, Some(1), 1, 2)
            .expect("the final candidate is always admitted")
            .expect("forced admission still holds a guard");
        drop((first, forced));
    }

    #[test]
    fn admission_release_frees_the_slot() {
        let pool = Arc::new(AccountPool::new());
        let acc = account("a");
        let guard = pool.clone().try_admit("codex", &acc, 1, false).unwrap();
        assert!(pool.clone().try_admit("codex", &acc, 1, false).is_none());
        drop(guard);
        assert!(
            pool.clone().try_admit("codex", &acc, 1, false).is_some(),
            "a released slot admits the next request"
        );
    }

    #[test]
    fn admission_is_shared_across_aliases_of_one_identity() {
        let pool = Arc::new(AccountPool::new());
        let alias_a = account_with_uuid("alias-a", "shared");
        let alias_b = account_with_uuid("alias-b", "shared");
        let guard = pool.clone().try_admit("codex", &alias_a, 1, false).unwrap();
        assert!(
            pool.clone()
                .try_admit("codex", &alias_b, 1, false)
                .is_none(),
            "aliases of one upstream identity share the admission gate"
        );
        drop(guard);
    }

    #[test]
    fn successful_responses_double_admission_allowance() {
        let pool = Arc::new(AccountPool::new());
        let acc = account("a");
        let guard = pool.clone().try_admit("codex", &acc, 1, false).unwrap();
        assert!(pool.clone().try_admit("codex", &acc, 1, false).is_none());
        pool.mark_healthy("codex", &acc, true);
        let second = pool
            .clone()
            .try_admit("codex", &acc, 1, false)
            .expect("a successful response doubles the allowance");
        assert!(pool.clone().try_admit("codex", &acc, 1, false).is_none());
        drop((guard, second));
    }

    #[test]
    fn relayed_client_errors_do_not_grow_admission_allowance() {
        let pool = Arc::new(AccountPool::new());
        let acc = account("a");
        let guard = pool.clone().try_admit("codex", &acc, 1, false).unwrap();
        pool.mark_healthy("codex", &acc, false);
        assert!(
            pool.clone().try_admit("codex", &acc, 1, false).is_none(),
            "a relayed client error must not grow the slow-start allowance"
        );
        drop(guard);
    }

    #[test]
    fn cooldown_restarts_the_admission_ramp() {
        let pool = Arc::new(AccountPool::new());
        let acc = account("a");
        let guard = pool.clone().try_admit("codex", &acc, 1, false).unwrap();
        pool.mark_healthy("codex", &acc, true);
        pool.mark_healthy("codex", &acc, true);
        pool.cooldown("codex", &acc, Duration::from_secs(1), "rate_limit");
        assert!(
            pool.clone().try_admit("codex", &acc, 1, false).is_none(),
            "after a cooldown the ramp restarts at the initial allowance"
        );
        drop(guard);
    }

    #[test]
    fn idle_identity_reenters_slow_start() {
        let pool = Arc::new(AccountPool::new());
        let acc = account("a");
        let guard = pool.clone().try_admit("codex", &acc, 2, false).unwrap();
        pool.mark_healthy("codex", &acc, true);
        drop(guard);
        {
            let mut entries = pool.entries.lock().expect("account health lock poisoned");
            let health = entries
                .get_mut(&("codex".to_string(), "a".to_string()))
                .expect("admitted identity has an entry");
            // Backdate the last activity beyond the idle-reset horizon; `None`
            // (an impossibly early instant) also counts as idle.
            health.ramp_last_activity = Instant::now().checked_sub(RAMP_IDLE_RESET);
        }
        let first = pool.clone().try_admit("codex", &acc, 2, false).unwrap();
        let second = pool.clone().try_admit("codex", &acc, 2, false).unwrap();
        assert!(
            pool.clone().try_admit("codex", &acc, 2, false).is_none(),
            "an idle identity re-enters slow start at the initial allowance"
        );
        drop((first, second));
    }

    #[test]
    fn forget_identity_is_provider_scoped() {
        let pool = AccountPool::new();
        let accounts = vec![account("main")];
        pool.cooldown(
            "anthropic",
            &accounts[0],
            Duration::from_secs(60),
            "transport",
        );
        pool.cooldown("codex", &accounts[0], Duration::from_secs(60), "transport");
        let old_codex_lock = pool.refresh_lock("codex", &accounts[0]);
        let anthropic_lock = pool.refresh_lock("anthropic", &accounts[0]);

        pool.forget_identity("codex", "main");

        let new_codex_lock = pool.refresh_lock("codex", &accounts[0]);
        assert!(!Arc::ptr_eq(&old_codex_lock, &new_codex_lock));
        assert!(Arc::ptr_eq(
            &anthropic_lock,
            &pool.refresh_lock("anthropic", &accounts[0])
        ));

        let codex = pool.snapshot("codex", &accounts, None, None);
        assert!(!codex[0].has_state);
        assert!(codex[0].available);

        let anthropic = pool.snapshot("anthropic", &accounts, None, None);
        assert!(anthropic[0].has_state);
        assert!(!anthropic[0].available);
        assert!(anthropic[0].cooldown_secs_remaining.is_some());
    }

    #[test]
    fn under_quota_accounts_sort_by_weekly_reset_with_unknown_first() {
        let pool = AccountPool::new();
        let accounts = vec![account("a"), account("b"), account("c"), account("d")];
        let session = "reset-sort";
        let rotation = pool.select_order("anthropic", &accounts, Some(session), None, None);
        let sticky = rotation[0];
        pool.note_quota(
            "anthropic",
            &accounts[sticky],
            &quota_headers(&[("anthropic-ratelimit-unified-status", "rejected".to_string())]),
        );
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let resets = [now + 300, now + 100, now + 200];
        for (position, (&index, reset)) in rotation[1..].iter().zip(resets).enumerate() {
            // Leave the first available account's reset unknown.
            if position != 0 {
                pool.note_quota(
                    "anthropic",
                    &accounts[index],
                    &quota_headers(&[("anthropic-ratelimit-unified-7d-reset", reset.to_string())]),
                );
            }
        }
        let selected = pool.select_order("anthropic", &accounts, Some(session), None, None);
        assert_eq!(selected[..3], [rotation[1], rotation[2], rotation[3]]);
        assert_eq!(selected[3], sticky);
    }

    #[test]
    fn fable_uses_oi_bucket_while_other_models_use_shared_weekly_bucket() {
        let pool = AccountPool::new();
        let accounts = accounts();
        let session = "model-aware";
        let rotation = pool.select_order("anthropic", &accounts, Some(session), None, None);
        let sticky = rotation[0];
        pool.note_quota(
            "anthropic",
            &accounts[sticky],
            &quota_headers(&[
                (
                    "anthropic-ratelimit-unified-7d-utilization",
                    "0.25".to_string(),
                ),
                (
                    "anthropic-ratelimit-unified-7d_oi-utilization",
                    "1.0".to_string(),
                ),
            ]),
        );
        assert_eq!(
            pool.select_order(
                "anthropic",
                &accounts,
                Some(session),
                Some("claude-opus-4-8"),
                None,
            )[0],
            sticky
        );
        assert_ne!(
            pool.select_order(
                "anthropic",
                &accounts,
                Some(session),
                Some("CLAUDE-FABLE-5"),
                None,
            )[0],
            sticky
        );
    }

    #[test]
    fn note_quota_parses_preserves_and_expires_fields() {
        let pool = AccountPool::new();
        let accounts = vec![account("a"), account("b")];
        let session = "expiry";
        let rotation = pool.select_order("anthropic", &accounts, Some(session), None, None);
        let sticky = rotation[0];
        let past = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(1);
        pool.note_quota(
            "anthropic",
            &accounts[sticky],
            &quota_headers(&[
                (
                    "anthropic-ratelimit-unified-5h-utilization",
                    "0.99".to_string(),
                ),
                ("anthropic-ratelimit-unified-5h-reset", past.to_string()),
                (
                    "anthropic-ratelimit-unified-7d-utilization",
                    "0.42".to_string(),
                ),
                (
                    "anthropic-ratelimit-unified-7d-reset",
                    "invalid".to_string(),
                ),
                ("anthropic-ratelimit-unified-status", "rejected".to_string()),
            ]),
        );

        let selected = pool.select_order("anthropic", &accounts, Some(session), None, None);
        assert_eq!(selected[0], sticky);
        let entries = pool.entries.lock().unwrap();
        let quota = &entries
            .get(&("anthropic".to_string(), accounts[sticky].name.clone()))
            .unwrap()
            .quota;
        assert_eq!(quota.utilization_5h, None);
        assert_eq!(quota.reset_5h, None);
        assert_eq!(quota.utilization_7d, Some(0.42));
        assert_eq!(quota.reset_7d, None);
        assert_eq!(quota.status, None);
    }

    #[test]
    fn note_usage_applies_snapshot_and_drives_selection() {
        let pool = AccountPool::new();
        let accounts = vec![account("a"), account("b")];
        let session = "usage";
        let rotation = pool.select_order("anthropic", &accounts, Some(session), None, None);
        let sticky = rotation[0];
        let future = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;

        // An authoritative usage snapshot puts the sticky account over the shared
        // weekly threshold, so the next selection must rotate away from it.
        pool.note_usage(
            "anthropic",
            &accounts[sticky],
            &UsageSnapshot {
                five_hour: Some(UsageWindow {
                    utilization: 0.33,
                    resets_at: Some(future),
                }),
                seven_day: Some(UsageWindow {
                    utilization: 0.99,
                    resets_at: Some(future),
                }),
                seven_day_oi: None,
            },
        );

        let snaps = pool.snapshot("anthropic", &accounts, None, None);
        let sticky_snap = snaps
            .iter()
            .find(|s| s.name == accounts[sticky].name)
            .unwrap();
        assert!(sticky_snap.has_state);
        assert!(sticky_snap.near_quota);
        assert_eq!(sticky_snap.utilization_7d, Some(0.99));
        assert_eq!(sticky_snap.utilization_5h, Some(0.33));
        assert_eq!(sticky_snap.reset_7d, Some(future));

        let rotated = pool.select_order("anthropic", &accounts, Some(session), None, None);
        assert_ne!(rotated[0], sticky);
    }

    #[test]
    fn note_usage_omitted_window_leaves_prior_header_value() {
        let pool = AccountPool::new();
        let accounts = [account("a")];
        // A prior header records a fable (7d_oi) utilization.
        pool.note_quota(
            "anthropic",
            &accounts[0],
            &quota_headers(&[(
                "anthropic-ratelimit-unified-7d_oi-utilization",
                "0.5".to_string(),
            )]),
        );
        // The usage snapshot reports only 5h/7d — the omitted 7d_oi survives.
        pool.note_usage(
            "anthropic",
            &accounts[0],
            &UsageSnapshot {
                five_hour: Some(UsageWindow {
                    utilization: 0.1,
                    resets_at: None,
                }),
                seven_day: Some(UsageWindow {
                    utilization: 0.2,
                    resets_at: None,
                }),
                seven_day_oi: None,
            },
        );
        let entries = pool.entries.lock().unwrap();
        let quota = &entries
            .get(&("anthropic".to_string(), "a".to_string()))
            .unwrap()
            .quota;
        assert_eq!(quota.utilization_5h, Some(0.1));
        assert_eq!(quota.utilization_7d, Some(0.2));
        assert_eq!(quota.utilization_7d_oi, Some(0.5));
    }

    #[test]
    fn cooldown_skips_accounts_and_all_cooled_uses_soonest_expiry() {
        let pool = AccountPool::new();
        let accounts = vec![account("a"), account("b"), account("c")];
        let sticky = pool.select_order("anthropic", &accounts, Some("sticky"), None, None)[0];
        pool.cooldown(
            "anthropic",
            &accounts[sticky],
            Duration::from_secs(30),
            "transport",
        );
        let available = pool.select_order("anthropic", &accounts, Some("sticky"), None, None);
        assert_eq!(available.len(), 3);
        assert_eq!(available[2], sticky);

        for (index, seconds) in [(0, 30), (1, 20), (2, 10)] {
            pool.cooldown(
                "anthropic",
                &accounts[index],
                Duration::from_secs(seconds),
                "transport",
            );
        }
        assert_eq!(
            pool.select_order("anthropic", &accounts, Some("sticky"), None, None),
            vec![2, 1, 0]
        );
    }

    #[test]
    fn round_robin_counters_are_independent_per_provider() {
        let pool = AccountPool::new();
        let accounts = accounts();
        assert_eq!(pool.select_order("one", &accounts, None, None, None)[0], 0);
        assert_eq!(pool.select_order("one", &accounts, None, None, None)[0], 1);
        assert_eq!(pool.select_order("two", &accounts, None, None, None)[0], 0);
        assert_eq!(pool.select_order("one", &accounts, None, None, None)[0], 2);
        assert_eq!(pool.select_order("two", &accounts, None, None, None)[0], 1);
    }

    #[test]
    fn classifies_upstream_responses() {
        let mut rejected = HeaderMap::new();
        rejected.insert(
            "anthropic-ratelimit-unified-5h-status",
            HeaderValue::from_static("rejected"),
        );
        assert_eq!(
            classify(StatusCode::TOO_MANY_REQUESTS, &rejected),
            FailoverAction::Rotate
        );
        assert_eq!(
            classify(StatusCode::TOO_MANY_REQUESTS, &HeaderMap::new()),
            FailoverAction::PauseSame
        );
        assert_eq!(
            classify(StatusCode::UNAUTHORIZED, &HeaderMap::new()),
            FailoverAction::RefreshRetry
        );
        assert_eq!(
            classify(StatusCode::SERVICE_UNAVAILABLE, &HeaderMap::new()),
            FailoverAction::Rotate
        );
        assert_eq!(
            classify(StatusCode::OK, &HeaderMap::new()),
            FailoverAction::Relay
        );
        assert_eq!(
            classify(StatusCode::BAD_REQUEST, &HeaderMap::new()),
            FailoverAction::Relay
        );
    }

    #[test]
    fn classifies_upstream_responses_codex() {
        // Codex quota/rejection headers are display-only, so every 429 rotates
        // rather than taking Anthropic's PauseSame path.
        assert_eq!(
            classify_codex(StatusCode::TOO_MANY_REQUESTS, &HeaderMap::new()),
            FailoverAction::Rotate
        );
        assert_eq!(
            classify_codex(StatusCode::UNAUTHORIZED, &HeaderMap::new()),
            FailoverAction::RefreshRetry
        );
        assert_eq!(
            classify_codex(StatusCode::SERVICE_UNAVAILABLE, &HeaderMap::new()),
            FailoverAction::Rotate
        );
        assert_eq!(
            classify_codex(StatusCode::OK, &HeaderMap::new()),
            FailoverAction::Relay
        );
        assert_eq!(
            classify_codex(StatusCode::BAD_REQUEST, &HeaderMap::new()),
            FailoverAction::Relay
        );
    }

    #[test]
    fn parses_numeric_retry_after() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("42"));
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(42)));
    }

    #[test]
    fn parses_http_date_retry_after() {
        // RFC 7231 date form: a deadline ~1h in the future is honored as a
        // positive wait rather than silently ignored (which would fall through
        // to computed backoff and retry before the server's requested deadline).
        let deadline = SystemTime::now() + Duration::from_secs(3600);
        let mut headers = HeaderMap::new();
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_str(&httpdate::fmt_http_date(deadline)).unwrap(),
        );
        let wait = retry_after(&headers).expect("http-date retry-after is honored");
        // HTTP-date has 1s resolution; allow a small slack around the ~3600s wait.
        assert!(
            wait <= Duration::from_secs(3600) && wait >= Duration::from_secs(3595),
            "expected ~3600s, got {wait:?}"
        );
    }

    #[test]
    fn past_http_date_retry_after_is_zero() {
        // A deadline already in the past means "retry now", not a fall-through.
        let deadline = SystemTime::now() - Duration::from_secs(3600);
        let mut headers = HeaderMap::new();
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_str(&httpdate::fmt_http_date(deadline)).unwrap(),
        );
        assert_eq!(retry_after(&headers), Some(Duration::ZERO));
    }

    #[test]
    fn unparseable_retry_after_is_none() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("not-a-date"));
        assert_eq!(retry_after(&headers), None);
    }

    fn unix_now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn threshold_resolution_prefers_most_specific_and_caps_at_hard() {
        let pool = PoolConfig {
            hard_threshold: 0.9,
            default_threshold: Some(0.5),
            default_threshold_5h: Some(0.6),
            ..Default::default()
        };
        let mut acct = account("a");
        // Pool defaults: the per-window value wins over the shared default.
        assert_eq!(
            resolved_threshold(QuotaWindow::FiveHour, &acct, Some(&pool)),
            0.6
        );
        assert_eq!(
            resolved_threshold(QuotaWindow::Weekly, &acct, Some(&pool)),
            0.5
        );
        assert_eq!(
            resolved_threshold(QuotaWindow::Fable, &acct, Some(&pool)),
            0.5
        );
        // An account-level threshold beats every pool default…
        acct.threshold = Some(0.7);
        assert_eq!(
            resolved_threshold(QuotaWindow::FiveHour, &acct, Some(&pool)),
            0.7
        );
        // …and the account's per-window value beats the account default, but
        // never escapes the hard backstop.
        acct.threshold_5h = Some(0.95);
        assert_eq!(
            resolved_threshold(QuotaWindow::FiveHour, &acct, Some(&pool)),
            0.9
        );
        // Without [server.pool] the account threshold still applies, capped at
        // the legacy 0.98 backstop; nothing configured resolves to the backstop.
        assert_eq!(resolved_threshold(QuotaWindow::Weekly, &acct, None), 0.7);
        assert_eq!(
            resolved_threshold(QuotaWindow::Weekly, &account("bare"), None),
            SWITCH_THRESHOLD
        );
    }

    #[test]
    fn window_headroom_projects_exhaustion_minus_reset() {
        let now = 1_000_000;
        // Already at/past the threshold: no headroom at all.
        assert_eq!(
            window_headroom(0.6, Some(now + 100), WINDOW_5H_SECS, 0.5, now),
            f64::NEG_INFINITY
        );
        // No usage yet, or no reset instant: no evidence of pressure.
        assert_eq!(
            window_headroom(0.0, Some(now + 100), WINDOW_5H_SECS, 0.5, now),
            f64::INFINITY
        );
        assert_eq!(
            window_headroom(0.4, None, WINDOW_5H_SECS, 0.5, now),
            f64::INFINITY
        );
        // Halfway through the 5h window at 0.25 of a 1.0 threshold: exhaustion
        // in 3× the elapsed 9000s, reset in 9000s → +18000s of margin.
        let headroom = window_headroom(0.25, Some(now + 9_000), WINDOW_5H_SECS, 1.0, now);
        assert!((headroom - 18_000.0).abs() < 1e-6, "got {headroom}");
        // 0.9 burned in the first 1800s of the window: the 0.98 threshold is
        // ~160s away but the reset is 16200s away → deeply negative.
        let headroom = window_headroom(0.9, Some(now + 16_200), WINDOW_5H_SECS, 0.98, now);
        assert!(headroom < -15_000.0, "got {headroom}");
    }

    #[test]
    fn account_threshold_override_rotates_backup_account_early() {
        let pool = AccountPool::new();
        let mut accounts = accounts();
        let session = "acct-threshold";
        let cfg = PoolConfig::default();
        let rotation = pool.select_order("anthropic", &accounts, Some(session), None, Some(&cfg));
        let sticky = rotation[0];
        // A backup account keeps a low personal threshold; 0.6 utilization is
        // fine for everyone else but "near" for it.
        accounts[sticky].threshold = Some(0.5);
        pool.note_quota(
            "anthropic",
            &accounts[sticky],
            &quota_headers(&[(
                "anthropic-ratelimit-unified-5h-utilization",
                "0.6".to_string(),
            )]),
        );
        let order = pool.select_order("anthropic", &accounts, Some(session), None, Some(&cfg));
        assert_ne!(order[0], sticky);
        assert_eq!(order.last(), Some(&sticky));
    }

    #[test]
    fn burn_rate_avoidance_rotates_fast_burning_sticky_account() {
        let pool = AccountPool::new();
        let accounts = accounts();
        let session = "burn-rate";
        let ordering_only = PoolConfig::default();
        let avoid = PoolConfig {
            burn_rate_avoidance: true,
            ..Default::default()
        };
        let rotation = pool.select_order(
            "anthropic",
            &accounts,
            Some(session),
            None,
            Some(&ordering_only),
        );
        let sticky = rotation[0];
        // 0.9 burned just 30 minutes into the 5h window: projected to exhaust
        // the backstop long before the reset 4.5h away.
        pool.note_quota(
            "anthropic",
            &accounts[sticky],
            &quota_headers(&[
                (
                    "anthropic-ratelimit-unified-5h-utilization",
                    "0.9".to_string(),
                ),
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    (unix_now() + 16_200).to_string(),
                ),
            ]),
        );
        // Headroom only orders; without avoidance the sticky account stays.
        assert_eq!(
            pool.select_order(
                "anthropic",
                &accounts,
                Some(session),
                None,
                Some(&ordering_only)
            )[0],
            sticky
        );
        let avoided = pool.select_order("anthropic", &accounts, Some(session), None, Some(&avoid));
        assert_ne!(avoided[0], sticky);
        assert_eq!(avoided.last(), Some(&sticky));
    }

    #[test]
    fn priority_orders_available_accounts_in_both_modes() {
        for cfg in [None, Some(PoolConfig::default())] {
            let pool = AccountPool::new();
            let mut accounts = accounts();
            let session = "priority";
            let rotation =
                pool.select_order("anthropic", &accounts, Some(session), None, cfg.as_ref());
            let sticky = rotation[0];
            pool.note_quota(
                "anthropic",
                &accounts[sticky],
                &quota_headers(&[(
                    "anthropic-ratelimit-unified-5h-utilization",
                    "0.99".to_string(),
                )]),
            );
            // Prefer what would otherwise be the last rotation slot.
            let preferred = *rotation.last().unwrap();
            accounts[preferred].priority = 1;
            let order =
                pool.select_order("anthropic", &accounts, Some(session), None, cfg.as_ref());
            assert_eq!(order[0], preferred, "pool config: {cfg:?}");
            assert_eq!(order.last(), Some(&sticky), "pool config: {cfg:?}");
        }
    }

    #[test]
    fn all_near_accounts_fall_back_to_headroom_order() {
        // Every account trips burn-rate avoidance: instead of emptying the
        // pool (or piling up in rotation order), selection degrades to
        // best-projected-margin first.
        let pool = AccountPool::new();
        let accounts = vec![account("a"), account("b"), account("c")];
        let cfg = PoolConfig {
            burn_rate_avoidance: true,
            ..Default::default()
        };
        let now = unix_now();
        // Same 0.9 utilization, increasingly distant resets: the further the
        // reset, the earlier in the window the burn happened and the worse the
        // projected margin.
        for (index, reset_in) in [(0usize, 16_200u64), (1, 9_000), (2, 3_600)] {
            pool.note_quota(
                "anthropic",
                &accounts[index],
                &quota_headers(&[
                    (
                        "anthropic-ratelimit-unified-5h-utilization",
                        "0.9".to_string(),
                    ),
                    (
                        "anthropic-ratelimit-unified-5h-reset",
                        (now + reset_in).to_string(),
                    ),
                ]),
            );
        }
        assert_eq!(
            pool.select_order("anthropic", &accounts, Some("all-near"), None, Some(&cfg)),
            vec![2, 1, 0]
        );
    }

    #[test]
    fn all_near_bucket_honors_priority_before_headroom() {
        // The near_soft fallback tiebreaks on priority first (mirroring
        // available_under): a configured primary stays preferred even when its
        // burn-rate headroom is the worst of the pool, so a backup never
        // overtakes it on a tiny margin slip.
        let pool = AccountPool::new();
        let mut accounts = vec![account("a"), account("b"), account("c")];
        let cfg = PoolConfig {
            burn_rate_avoidance: true,
            ..Default::default()
        };
        let now = unix_now();
        // Same utilization, resets chosen so headroom order alone would sort
        // [2, 1, 0] (account 0 last — see the test above).
        for (index, reset_in) in [(0usize, 16_200u64), (1, 9_000), (2, 3_600)] {
            pool.note_quota(
                "anthropic",
                &accounts[index],
                &quota_headers(&[
                    (
                        "anthropic-ratelimit-unified-5h-utilization",
                        "0.9".to_string(),
                    ),
                    (
                        "anthropic-ratelimit-unified-5h-reset",
                        (now + reset_in).to_string(),
                    ),
                ]),
            );
        }
        // Designate the worst-headroom account as the primary: priority wins.
        accounts[0].priority = 1;
        assert_eq!(
            pool.select_order("anthropic", &accounts, Some("all-near"), None, Some(&cfg)),
            vec![0, 2, 1]
        );
    }

    #[test]
    fn available_accounts_order_by_burn_rate_headroom() {
        // With [server.pool] set, equal-priority accounts still under their soft
        // threshold order by largest projected headroom first — the headline
        // burn-rate-aware ordering. (Distinct from the near_soft bucket, which
        // all_near_accounts_fall_back_to_headroom_order covers.)
        let pool = AccountPool::new();
        let accounts = vec![account("a"), account("b"), account("c")];
        let cfg = PoolConfig::default();
        let session = "avail-headroom";
        let now = unix_now();
        let rotation = pool.select_order("anthropic", &accounts, Some(session), None, Some(&cfg));
        let sticky = rotation[0];
        // Push the sticky account near quota so the available_under sort runs
        // (a healthy sticky account short-circuits to rotation order).
        pool.note_quota(
            "anthropic",
            &accounts[sticky],
            &quota_headers(&[(
                "anthropic-ratelimit-unified-5h-utilization",
                "0.99".to_string(),
            )]),
        );
        // Both remaining accounts stay well under threshold (0.3) but burn at
        // different rates: the nearer reset means more of the window has already
        // elapsed, a slower observed pace, and thus larger headroom.
        let others: Vec<usize> = (0..accounts.len()).filter(|&i| i != sticky).collect();
        let (slow, fast) = (others[0], others[1]);
        pool.note_quota(
            "anthropic",
            &accounts[slow],
            &quota_headers(&[
                (
                    "anthropic-ratelimit-unified-5h-utilization",
                    "0.3".to_string(),
                ),
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    (now + 3_600).to_string(),
                ),
            ]),
        );
        pool.note_quota(
            "anthropic",
            &accounts[fast],
            &quota_headers(&[
                (
                    "anthropic-ratelimit-unified-5h-utilization",
                    "0.3".to_string(),
                ),
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    (now + 16_200).to_string(),
                ),
            ]),
        );
        let order = pool.select_order("anthropic", &accounts, Some(session), None, Some(&cfg));
        assert_eq!(order[0], slow, "larger-headroom account sorts first");
        assert_eq!(order[1], fast, "faster-burning account sorts after");
        assert_eq!(order.last(), Some(&sticky), "near sticky sorts last");
    }

    #[test]
    fn accounts_past_hard_threshold_sort_after_soft_near_accounts() {
        let pool = AccountPool::new();
        let accounts = accounts();
        let session = "hard-backstop";
        let cfg = PoolConfig {
            default_threshold: Some(0.5),
            ..Default::default()
        };
        let rotation = pool.select_order("anthropic", &accounts, Some(session), None, Some(&cfg));
        // Sticky account past the hard backstop, the next one past only the
        // soft threshold, the rest untouched.
        for (offset, utilization) in [(0usize, "0.99"), (1, "0.6")] {
            pool.note_quota(
                "anthropic",
                &accounts[rotation[offset]],
                &quota_headers(&[(
                    "anthropic-ratelimit-unified-5h-utilization",
                    utilization.to_string(),
                )]),
            );
        }
        let order = pool.select_order("anthropic", &accounts, Some(session), None, Some(&cfg));
        assert_eq!(order[..2], [rotation[2], rotation[3]]);
        assert_eq!(order[2], rotation[1], "soft-near sorts before hard-over");
        assert_eq!(order[3], rotation[0], "hard-over sorts last");
    }

    #[test]
    fn disabled_accounts_are_excluded_from_selection() {
        for cfg in [None, Some(PoolConfig::default())] {
            let pool = AccountPool::new();
            let mut accounts = accounts();
            let session = "disabled";
            let rotation =
                pool.select_order("anthropic", &accounts, Some(session), None, cfg.as_ref());
            let sticky = rotation[0];
            accounts[sticky].disabled = true;
            let order =
                pool.select_order("anthropic", &accounts, Some(session), None, cfg.as_ref());
            assert_eq!(order.len(), 3, "pool config: {cfg:?}");
            assert!(!order.contains(&sticky), "pool config: {cfg:?}");
        }
    }

    #[test]
    fn all_disabled_accounts_yield_empty_order() {
        // A non-empty account list with every account disabled selects nothing
        // (callers turn this into a distinct config error rather than a generic
        // "all accounts failed").
        for cfg in [None, Some(PoolConfig::default())] {
            let pool = AccountPool::new();
            let mut accounts = accounts();
            for account in &mut accounts {
                account.disabled = true;
            }
            let order = pool.select_order(
                "anthropic",
                &accounts,
                Some("all-disabled"),
                None,
                cfg.as_ref(),
            );
            assert!(order.is_empty(), "pool config: {cfg:?}");
        }
    }

    #[test]
    fn snapshot_reports_pool_fields() {
        let pool = AccountPool::new();
        let mut accounts = vec![account("seen"), account("standby")];
        accounts[1].disabled = true;
        accounts[1].priority = 200;
        pool.note_quota(
            "anthropic",
            &accounts[0],
            &quota_headers(&[
                (
                    "anthropic-ratelimit-unified-5h-utilization",
                    "0.5".to_string(),
                ),
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    (unix_now() + 9_000).to_string(),
                ),
            ]),
        );
        let cfg = PoolConfig::default();
        let snaps = pool.snapshot("anthropic", &accounts, None, Some(&cfg));
        let seen = &snaps[0];
        assert_eq!(seen.priority, 100, "the default priority");
        assert!(!seen.disabled);
        assert!(
            seen.headroom_secs.is_some(),
            "finite projection is reported with [server.pool] set"
        );
        let standby = &snaps[1];
        assert!(standby.disabled);
        assert_eq!(standby.priority, 200);
        assert!(!standby.available, "a disabled account is never available");
        // Without [server.pool], the projection is not surfaced.
        let legacy = pool.snapshot("anthropic", &accounts, None, None);
        assert!(legacy[0].headroom_secs.is_none());
    }

    #[test]
    fn export_import_round_trips_quota() {
        let pool = AccountPool::new();
        let acct = account("a");
        pool.note_quota(
            "anthropic",
            &acct,
            &quota_headers(&[
                (
                    "anthropic-ratelimit-unified-5h-utilization",
                    "0.5".to_string(),
                ),
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    (unix_now() + 9_000).to_string(),
                ),
            ]),
        );
        let exported = pool.export_quotas();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].0, "anthropic");
        assert_eq!(exported[0].1, "a");

        // A fresh pool seeded from the export reports the same utilization and
        // re-exports an identical snapshot.
        let restored = AccountPool::new();
        restored.import_quotas(exported.clone());
        let snaps = restored.snapshot("anthropic", &[acct], None, None);
        assert!(snaps[0].has_state);
        assert_eq!(snaps[0].utilization_5h, Some(0.5));
        assert_eq!(restored.export_quotas(), exported);
    }

    #[test]
    fn export_import_round_trips_status_only_quota() {
        let pool = AccountPool::new();
        pool.import_quotas([(
            "anthropic".to_string(),
            "a".to_string(),
            QuotaState {
                status: Some("rejected".to_string()),
                ..Default::default()
            },
        )]);

        let exported = pool.export_quotas();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].2.status.as_deref(), Some("rejected"));

        let restored = AccountPool::new();
        restored.import_quotas(exported);
        let snapshots = restored.snapshot("anthropic", &[account("a")], None, None);
        assert!(snapshots[0].near_quota);
        assert_eq!(snapshots[0].status.as_deref(), Some("rejected"));
    }

    #[test]
    fn export_import_round_trips_reset_only_quota() {
        let reset = unix_now() + 9_000;
        let pool = AccountPool::new();
        pool.import_quotas([(
            "anthropic".to_string(),
            "a".to_string(),
            QuotaState {
                reset_7d: Some(reset),
                ..Default::default()
            },
        )]);

        let exported = pool.export_quotas();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].2.reset_7d, Some(reset));

        let restored = AccountPool::new();
        restored.import_quotas(exported);
        let snapshots = restored.snapshot("anthropic", &[account("a")], None, None);
        assert_eq!(snapshots[0].reset_7d, Some(reset));
    }

    #[test]
    fn export_skips_accounts_without_quota_signal() {
        // A cooldown marks the account observed but records no quota, so there
        // is nothing worth persisting.
        let pool = AccountPool::new();
        pool.cooldown("anthropic", &account("a"), Duration::from_secs(60), "test");
        assert!(pool.export_quotas().is_empty());
    }

    #[test]
    fn forgetting_persisted_quota_marks_dirty_and_removes_export() {
        let pool = AccountPool::new();
        pool.import_quotas([(
            "anthropic".to_string(),
            "a".to_string(),
            QuotaState {
                utilization_5h: Some(0.5),
                ..Default::default()
            },
        )]);
        assert!(!pool.take_dirty(), "restored quota starts clean");
        assert_eq!(pool.export_quotas().len(), 1);

        pool.forget_identity("anthropic", "a");

        assert!(pool.take_dirty(), "removing persisted quota marks dirty");
        assert!(pool.export_quotas().is_empty());
    }

    #[test]
    fn forgetting_cooldown_only_state_does_not_mark_dirty() {
        let pool = AccountPool::new();
        pool.cooldown("anthropic", &account("a"), Duration::from_secs(60), "test");

        pool.forget_identity("anthropic", "a");

        assert!(!pool.take_dirty(), "cooldowns are not persisted");
    }

    #[test]
    fn quota_mutation_marks_dirty_and_take_clears_it() {
        let pool = AccountPool::new();
        assert!(!pool.take_dirty(), "a fresh pool is clean");
        pool.note_quota(
            "anthropic",
            &account("a"),
            &quota_headers(&[(
                "anthropic-ratelimit-unified-5h-utilization",
                "0.5".to_string(),
            )]),
        );
        assert!(pool.take_dirty(), "a quota mutation marks the pool dirty");
        assert!(!pool.take_dirty(), "take_dirty clears the flag");
    }
}
