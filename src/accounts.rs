use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use reqwest::{header::HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;

use crate::config::{AccountConfig, PoolConfig};

/// Credential-store namespace. Stable account ids only coalesce inside their
/// own store family, so a Claude UUID can never collide with a ChatGPT account id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreFamily {
    Claude,
    Chatgpt,
    Kimi,
}

/// Stable physical-account identity used by every runtime state map.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum AccountStateIdentity {
    Verified { id: String },
    StoreEntry { name: String },
    UpstreamInline { upstream: String, name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct AccountKey {
    pub(crate) store_family: StoreFamily,
    pub(crate) identity: AccountStateIdentity,
}

type RefreshLock = Arc<AsyncMutex<()>>;

/// Legacy hard threshold, used verbatim when `[server.pool]` is not
/// configured so selection behaves exactly as it did before issue #135.
const SWITCH_THRESHOLD: f64 = 0.98;

/// Window lengths are hardcoded because the quota headers carry only the
/// reset instant, never the window size (issue #135).
const WINDOW_5H_SECS: u64 = 5 * 60 * 60;
const WINDOW_7D_SECS: u64 = 7 * 24 * 60 * 60;

/// Default opportunistic-reprobe interval when `[server.pool]` is configured
/// but `reprobe_seconds` is unset. See [`reprobe_interval`].
const REPROBE_DEFAULT_SECS: u64 = 900;
/// Minimum positive opportunistic-reprobe interval.
pub(crate) const REPROBE_FLOOR_SECS: u64 = 60;

/// The effective opportunistic-reprobe interval, or `None` when re-probing is
/// disabled. Disabled when `[server.pool]` itself is absent (preserves the
/// documented pre-#135 behavior: no pool config, no probing), or when
/// `reprobe_seconds` is explicitly `0`. Unset with a pool present defaults to
/// [`REPROBE_DEFAULT_SECS`]. The outbound Responses pool separately suppresses
/// re-probing when WebSocket transport is enabled for the provider.
fn reprobe_interval(pool: Option<&PoolConfig>) -> Option<Duration> {
    match pool?.reprobe_seconds {
        Some(0) => None,
        // Positive values below 60 are clamped up to a 60-second floor, same
        // as `usage_refresh_seconds`. This is the single read site for
        // `reprobe_seconds` (`select_order_deferred` calls this on HTTP pool
        // requests); the operator-facing warning is emitted once after a
        // successful config load.
        Some(seconds) => Some(Duration::from_secs(seconds.max(REPROBE_FLOOR_SECS))),
        None => Some(Duration::from_secs(REPROBE_DEFAULT_SECS)),
    }
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_5h: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_7d: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_7d_oi: Option<String>,
    /// Unix time this window's utilization was last recorded. Bounds a
    /// reset-less utilization lifetime when no reset instant is available.
    /// Never feeds `window_headroom` or `assess_quota`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_5h: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_7d: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_7d_oi: Option<u64>,
    /// Unix time this window's per-window status was last recorded. Status
    /// freshness is independent from utilization freshness because usage
    /// polling does not report upstream rejection status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_status_5h: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_status_7d: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_status_7d_oi: Option<u64>,
    /// Reset boundary captured when the matching per-window status was
    /// observed. Later usage or reset-only updates must not extend that
    /// status's lifetime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at_status_5h: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at_status_7d: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at_status_7d_oi: Option<u64>,
    /// Unix time the aggregate `status` was last recorded. Stamped
    /// independently of the per-window `observed_at_*` fields: aggregate
    /// `status` is the only signal `assess_quota`'s `has_window_status`
    /// fallback reads when no per-window status is present, so it needs its
    /// own unconditional lifetime cap in `expire_stale_quota` — otherwise a
    /// window signal kept fresh by something else (e.g. a usage poller that
    /// never touches `status`) could keep a stale aggregate rejection from
    /// ever expiring on its own. A value recorded at runtime is a real
    /// aggregate-status observation time. A value persisted in v3 may instead
    /// be the synthetic deadline encoding produced by an earlier v2 migration;
    /// normal v3 import must preserve it rather than reinterpret reset
    /// metadata. Normal import still normalizes orphan metadata, expires
    /// elapsed signals, clamps future timestamps to boot time, and supplies
    /// boot time when a surviving aggregate is unstamped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at_status: Option<u64>,
}

impl QuotaState {
    /// Whether any persisted quota field carries a recorded signal. Utilization,
    /// reset metadata, and aggregate or per-window status all affect selection or
    /// diagnostics, so only an entirely default quota is omitted from persistence.
    /// The `observed_at_*` fields are deliberately not checked here: normally
    /// each is cleared alongside the window or aggregate signal it stamps (see
    /// `expire_stale_quota`). Import can briefly leave a clamped future stamp
    /// as the sole live field on a signal-free quota; that account is
    /// intentionally omitted from persistence.
    pub(crate) fn has_signal(&self) -> bool {
        self.utilization_5h.is_some()
            || self.reset_5h.is_some()
            || self.utilization_7d.is_some()
            || self.reset_7d.is_some()
            || self.utilization_7d_oi.is_some()
            || self.reset_7d_oi.is_some()
            || self.status.is_some()
            || self.status_5h.is_some()
            || self.status_7d.is_some()
            || self.status_7d_oi.is_some()
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

/// Why an account carries the needs-re-login mark. The distinction matters for
/// exactly one decision: whether a *successful refresh grant* is enough to clear
/// the mark. It is never surfaced to clients — [`AccountSnapshot::needs_relogin`]
/// stays a boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloginCause {
    /// The refresh grant itself failed terminally — `invalid_grant`, no stored
    /// refresh token, or a rotated pair lost before it could be persisted. A
    /// later grant that succeeds disproves this, so the admin refresh probe may
    /// clear it.
    RefreshGrant,
    /// The provider rejected a bearer the account actually presented to the API:
    /// a 401 on a credential that cannot be refreshed, or on the retry after a
    /// refresh that itself succeeded. A working grant says nothing about this —
    /// the grant was never the broken part — so only a served response clears it.
    ServedRequest,
}

impl ReloginCause {
    /// Fold a new verdict into whatever the entry already carried, keeping the
    /// stronger evidence. `ServedRequest` outranks `RefreshGrant`: a bearer the
    /// provider rejected in a real request says something a failing grant does
    /// not, and only a served response can disprove it.
    ///
    /// Without this, a terminal admin probe on an account the proxy had already
    /// condemned would rewrite `ServedRequest` as `RefreshGrant` — and the next
    /// grant that happened to succeed would then clear the mark, even though
    /// nothing had shown the account could serve inference again.
    fn strongest(existing: Option<Self>, incoming: Self) -> Self {
        match existing {
            Some(Self::ServedRequest) => Self::ServedRequest,
            _ => incoming,
        }
    }
}

#[derive(Debug, Default)]
struct AccountHealth {
    cooldown_until: Option<Instant>,
    cooldown_until_fable: Option<Instant>,
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
    /// Instant this identity was last dispatched for an opportunistic re-probe
    /// (see [`AccountPool::select_order_deferred`]). Memory-only, like
    /// `cooldown_until`: a restart just means the next stale-check treats the
    /// account as never probed, which is the safe default.
    last_probe_at: Option<Instant>,
    /// Opaque token for a stale-account promotion whose HTTP dispatch has not
    /// started yet. This is separate from `last_probe_at`: selection may hold
    /// this token while admission or credential resolution waits, but only an
    /// actual HTTP send consumes it and stamps the completed-probe interval.
    reprobe_reservation: Option<u64>,
    /// The account's credential is dead and only a re-login can revive it. Set
    /// on a terminal refresh failure, on a 401 against a credential that cannot
    /// be refreshed at all (`token_env`, a long-lived setup token), and on a
    /// 401 against the retry after a refresh that itself succeeded. The
    /// [`ReloginCause`] records which, so the admin probe knows whether its own
    /// successful grant is evidence of recovery.
    ///
    /// Deliberately **independent of `cooldown_until`**: a cooldown expires on
    /// its own after five minutes and the account is retried, which is exactly
    /// why a dead account otherwise cycles in and out of cooldown forever with
    /// nothing durable for an operator to see. This flag is the durable signal,
    /// and it is set only on a *terminal* failure — never on a transient one,
    /// which would report a healthy account as dead after a provider blip.
    ///
    /// Memory-only, like `cooldown_until`: the on-disk `[server.pool]
    /// state_path` persists quota alone, so a restart clears this and the
    /// account's next terminal failure re-establishes it.
    needs_relogin: Option<ReloginCause>,
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
    /// Seconds until the account-wide cooldown expires, when active.
    pub cooldown_secs_remaining: Option<u64>,
    /// Seconds until the Fable-only cooldown expires, when active.
    pub cooldown_fable_secs_remaining: Option<u64>,
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
    /// The credential is dead and needs an operator re-login (see
    /// [`AccountHealth::needs_relogin`]). Reported alongside — not folded
    /// into — `available` and the cooldown fields, so the dashboard can tell
    /// "cooling down, will retry" apart from "cooling down forever".
    pub needs_relogin: bool,
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
            cooldown_fable_secs_remaining: None,
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
            needs_relogin: false,
        }
    }
}

/// Process-lifetime health and scheduling state for configured accounts.
#[derive(Debug, Default)]
pub struct AccountPool {
    entries: Mutex<HashMap<AccountKey, AccountHealth>>,
    rr: Mutex<HashMap<String, usize>>,
    refresh_locks: Mutex<HashMap<AccountKey, RefreshLock>>,
    memberships: Mutex<HashMap<String, HashMap<AccountKey, bool>>>,
    /// Monotonic source for opaque in-flight reprobe reservations. Tokens are
    /// allocated while the entries lock is held, so concurrent selections
    /// cannot reserve one account with an ambiguous token.
    next_reprobe_token: AtomicU64,
    /// Set whenever a quota mutation lands, cleared by [`Self::take_dirty`].
    /// Lets the opt-in on-disk persister (see [`crate::state_persist`]) flush
    /// only when quota actually changed, rather than on every timer tick.
    dirty: AtomicBool,
}

#[derive(Debug)]
struct PendingReprobe {
    index: usize,
    token: u64,
    key: AccountKey,
    provider: String,
    account_name: String,
}

/// Deferred accounting for one stale-account promotion. Selection reserves an
/// account, while the caller commits only at the first actual HTTP send. The
/// reservation owns the pool so dropping it can safely cancel a still-matching
/// token without recreating an entry that was removed in the meantime.
#[derive(Debug)]
pub(crate) struct ReprobeReservation {
    pool: Arc<AccountPool>,
    token: u64,
    key: AccountKey,
    selected_index: usize,
    provider: String,
    account_name: String,
    finished: bool,
}

impl ReprobeReservation {
    fn new(pool: Arc<AccountPool>, pending: PendingReprobe) -> Self {
        Self {
            pool,
            token: pending.token,
            key: pending.key,
            selected_index: pending.index,
            provider: pending.provider,
            account_name: pending.account_name,
            finished: false,
        }
    }

    pub(crate) fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// Commit this reservation at the first HTTP dispatch boundary. Only a
    /// matching token may clear the pending state and stamp the actual send
    /// time. A stale or already-cancelled token is consumed without recreating
    /// an account entry and emits no metric or log.
    pub(crate) fn commit(&mut self) -> bool {
        if self.finished {
            return false;
        }
        self.finished = true;
        let committed = {
            let mut entries = self
                .pool
                .entries
                .lock()
                .expect("account health lock poisoned");
            let Some(health) = entries.get_mut(&self.key) else {
                return false;
            };
            if health.reprobe_reservation != Some(self.token) {
                return false;
            }
            health.reprobe_reservation = None;
            health.last_probe_at = Some(Instant::now());
            true
        };
        if committed {
            tracing::info!(
                provider = %self.provider,
                account = %self.account_name,
                "opportunistically re-probing a stale near-quota account"
            );
            crate::metrics::record_pool_reprobe(&self.provider);
        }
        committed
    }

    /// Explicitly cancel this reservation. Cancellation only clears the token
    /// that this selection installed; it never creates a missing health entry.
    pub(crate) fn cancel(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.pool.cancel_reprobe_token(&self.key, self.token);
    }
}

impl Drop for ReprobeReservation {
    fn drop(&mut self) {
        if !self.finished {
            self.pool.cancel_reprobe_token(&self.key, self.token);
        }
    }
}

impl AccountPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Synchronize the upstream's current configured identities before an
    /// out-of-band usage poll updates individual accounts. Membership remains
    /// upstream-scoped even though each identity's health is global.
    pub(crate) fn sync_enabled_accounts(&self, upstream: &str, accounts: &[AccountConfig]) {
        let mut membership = HashMap::new();
        for account in accounts {
            *membership
                .entry(account_key(upstream, account))
                .or_default() |= !account.disabled;
        }
        self.memberships
            .lock()
            .expect("account membership lock poisoned")
            .insert(upstream.to_string(), membership);
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
        self.select_order_inner(provider, accounts, session_id, model, pool, false)
            .0
    }

    /// Return account indices and, when one stale near-quota ChatGPT account
    /// was promoted, an opaque reservation for the first HTTP dispatch. The
    /// reservation does not consume the reprobe interval until the caller
    /// commits it immediately before sending upstream. Dropping it cancels the
    /// pending token, so admission and credential-resolution failures remain
    /// immediately eligible for a later request.
    pub(crate) fn select_order_deferred(
        self: &Arc<Self>,
        provider: &str,
        accounts: &[AccountConfig],
        session_id: Option<&str>,
        model: Option<&str>,
        pool: Option<&PoolConfig>,
    ) -> (Vec<usize>, Option<ReprobeReservation>) {
        let (order, pending) =
            self.select_order_inner(provider, accounts, session_id, model, pool, true);
        let reservation = pending.map(|pending| ReprobeReservation::new(Arc::clone(self), pending));
        (order, reservation)
    }

    /// Return account indices without opportunistic re-probing.
    ///
    /// Responses pools use this entry point when WebSocket transport is
    /// enabled. An in-stream rate-limit error arrives as a normal event, so
    /// the pool does not rotate; the streaming path then calls `mark_healthy`,
    /// which clears the cooldown and, when the turn is treated as successful
    /// and the account already has a positive ramp allowance, doubles that
    /// allowance. That contamination predates re-probing, and this entry point
    /// only removes re-probing as its new trigger while the deeper fix remains
    /// deferred. The provider-labelled re-probe metric therefore counts only
    /// inbound probes for providers with WebSocket enabled.
    pub(crate) fn select_order_without_reprobe(
        &self,
        provider: &str,
        accounts: &[AccountConfig],
        session_id: Option<&str>,
        model: Option<&str>,
        pool: Option<&PoolConfig>,
    ) -> Vec<usize> {
        self.select_order_inner(provider, accounts, session_id, model, pool, false)
            .0
    }

    #[cfg(test)]
    pub(crate) fn last_probe_at_for_test(
        &self,
        provider: &str,
        account: &AccountConfig,
    ) -> Option<Instant> {
        self.entries
            .lock()
            .expect("account health lock poisoned")
            .get(&account_key(provider, account))
            .and_then(|health| health.last_probe_at)
    }

    fn select_order_inner(
        &self,
        provider: &str,
        accounts: &[AccountConfig],
        session_id: Option<&str>,
        model: Option<&str>,
        pool: Option<&PoolConfig>,
        allow_reprobe: bool,
    ) -> (Vec<usize>, Option<PendingReprobe>) {
        if accounts.is_empty() {
            return (Vec::new(), None);
        }

        let provider = provider.to_string();
        self.sync_enabled_accounts(&provider, accounts);
        let ident_reps = collapse_representatives(&provider, accounts);
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

        // The sticky/round-robin slot is computed over distinct identities so
        // adding or removing an alias cannot move an existing session. Disabled
        // aliases yield to an enabled representative; fully disabled identities
        // are then dropped from the rotation entirely. `collapse_representatives`
        // and `rotation` need no lock, so both are computed before the entries
        // lock below — the opportunistic re-probe candidate (Change B) is
        // selected only from these final representatives.
        let rotation = (0..distinct)
            .map(|offset| ident_reps[(start_slot + offset) % distinct])
            .filter(|&index| !accounts[index].disabled)
            .collect::<Vec<_>>();

        let now = Instant::now();
        let unix_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let is_fable = is_fable_model(model);
        let reprobe = allow_reprobe.then(|| reprobe_interval(pool)).flatten();
        let (snapshots, pending_reprobe, quota_expired) = {
            let mut entries = self.entries.lock().expect("account health lock poisoned");
            let mut snapshots = Vec::with_capacity(accounts.len());
            let mut quota_expired = false;
            for account in accounts {
                let health = entries.entry(account_key(&provider, account)).or_default();
                health.enabled |= !account.disabled;
                quota_expired |= expire_stale_quota(&mut health.quota, unix_now);
                // Assessing under the lock is pure CPU work and avoids cloning
                // each account's QuotaState just to assess it after release.
                let assessment = assess_quota(&health.quota, account, is_fable, pool, unix_now);
                let weekly_reset = governing_weekly_reset(&health.quota, is_fable);
                let cooldown_until = governing_cooldown(health, is_fable);
                snapshots.push((cooldown_until, assessment, weekly_reset));
            }
            // Opportunistic re-probe (Change B): among the final rotation
            // representatives, find the single stale near-quota ChatGPT-family
            // account and reserve it while still holding the entries lock.
            // `last_probe_at` is deliberately not changed here: admission and
            // credential resolution can fail before any upstream request is
            // sent, so only the dispatch boundary may consume the interval.
            let probe_selection = reprobe.and_then(|interval| {
                let mut candidate: Option<(usize, Option<u64>)> = None;
                for &index in &rotation {
                    let account = &accounts[index];
                    // Family is read from the stamped field directly, never
                    // through `account_key`'s name-heuristic fallback: an
                    // unstamped account (`None`) is fail-safe ineligible, and
                    // only the codex/ChatGPT family tolerates a probe request
                    // landing on an account that turns out still exhausted
                    // (`classify_codex` rotates immediately on every 429;
                    // Claude/Kimi can `PauseSame` a probe for up to 300s).
                    if account.store_family != Some(StoreFamily::Chatgpt) {
                        continue;
                    }
                    let (cooldown_until, ref assessment, _) = snapshots[index];
                    if cooldown_until.is_some_and(|until| until > now) || !assessment.near {
                        continue;
                    }
                    let health = entries
                        .get(&account_key(&provider, account))
                        .expect("snapshot pass above just inserted this entry");
                    if health.reprobe_reservation.is_some() {
                        continue;
                    }
                    if health
                        .last_probe_at
                        .is_some_and(|at| now.saturating_duration_since(at) < interval)
                    {
                        continue;
                    }
                    // Freshness is the newest observation for each logical
                    // window, combining its utilization and status stamps,
                    // plus the independent aggregate status observation. An
                    // account with no stamps at all has never been observed
                    // and is treated as infinitely old, i.e. always eligible.
                    let freshness = [
                        health
                            .quota
                            .observed_at_5h
                            .max(health.quota.observed_at_status_5h),
                        health
                            .quota
                            .observed_at_7d
                            .max(health.quota.observed_at_status_7d),
                        health
                            .quota
                            .observed_at_7d_oi
                            .max(health.quota.observed_at_status_7d_oi),
                        health.quota.observed_at_status,
                    ]
                    .into_iter()
                    .flatten()
                    .max();
                    if let Some(at) = freshness {
                        if at.saturating_add(interval.as_secs()) > unix_now {
                            continue;
                        }
                    }
                    // `None` sorts before every `Some`, so this naturally
                    // prefers a never-observed account over any stale one.
                    if candidate.is_none_or(|(_, current)| freshness < current) {
                        candidate = Some((index, freshness));
                    }
                }
                candidate.map(|(index, _)| index)
            });

            let pending_reprobe = probe_selection.map(|index| {
                let account = &accounts[index];
                let key = account_key(&provider, account);
                let health = entries
                    .get_mut(&key)
                    .expect("snapshot pass above just inserted this entry");
                let token = self.next_reprobe_token.fetch_add(1, Ordering::Relaxed);
                health.reprobe_reservation = Some(token);
                PendingReprobe {
                    index,
                    token,
                    key,
                    provider: provider.clone(),
                    account_name: account.name.clone(),
                }
            });

            (snapshots, pending_reprobe, quota_expired)
        };

        if quota_expired {
            self.mark_dirty();
        }

        // Promotes the re-probe candidate, if any, to the front of a final
        // selection order — including the sticky fast path below, so a probe
        // is never starved by a healthy sticky account.
        let promote = |mut order: Vec<usize>| -> Vec<usize> {
            if let Some(probe) = pending_reprobe.as_ref().map(|pending| pending.index) {
                let position = order.iter().position(|&index| index == probe);
                debug_assert!(
                    position.is_some(),
                    "probe candidate must be present in the selection order"
                );
                if let Some(position) = position {
                    order.remove(position);
                    order.insert(0, probe);
                }
            }
            order
        };

        let sticky = ident_reps[start_slot];
        let (sticky_cooldown, ref sticky_quota, _) = snapshots[sticky];
        if !accounts[sticky].disabled
            && sticky_cooldown.is_none_or(|until| until <= now)
            && !sticky_quota.near
        {
            return (promote(rotation), pending_reprobe);
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

        (
            promote(
                available_under
                    .into_iter()
                    .chain(near_soft)
                    .chain(over_hard)
                    .chain(cooled)
                    .collect(),
            ),
            pending_reprobe,
        )
    }

    pub fn note_quota(&self, provider: &str, account: &AccountConfig, headers: &HeaderMap) {
        {
            let mut entries = self.entries.lock().expect("account health lock poisoned");
            let health = entries.entry(account_key(provider, account)).or_default();
            health.observed = true;
            let quota = &mut health.quota;
            let now = unix_now();

            // A response may carry the next window's reset after the old one
            // has passed. Expire the old state before any header can replace
            // that boundary.
            expire_stale_quota(quota, now);

            let wrote_utilization_5h = update_header(
                headers,
                "anthropic-ratelimit-unified-5h-utilization",
                &mut quota.utilization_5h,
            );
            let reset_5h = header_value::<u64>(headers, "anthropic-ratelimit-unified-5h-reset");
            let wrote_utilization_7d = update_header(
                headers,
                "anthropic-ratelimit-unified-7d-utilization",
                &mut quota.utilization_7d,
            );
            let reset_7d = header_value::<u64>(headers, "anthropic-ratelimit-unified-7d-reset");
            let wrote_utilization_7d_oi = update_header(
                headers,
                "anthropic-ratelimit-unified-7d_oi-utilization",
                &mut quota.utilization_7d_oi,
            );
            let reset_7d_oi =
                header_value::<u64>(headers, "anthropic-ratelimit-unified-7d_oi-reset");
            let wrote_status_5h =
                update_string_header(headers, QUOTA_STATUS_HEADERS[0], &mut quota.status_5h);
            let wrote_status_7d =
                update_string_header(headers, QUOTA_STATUS_HEADERS[1], &mut quota.status_7d);
            let wrote_status_7d_oi =
                update_string_header(headers, QUOTA_STATUS_HEADERS[2], &mut quota.status_7d_oi);
            let wrote_status = update_string_header(
                headers,
                "anthropic-ratelimit-unified-status",
                &mut quota.status,
            );

            if wrote_utilization_5h || wrote_status_5h {
                quota.reset_5h = preserve_future_reset(quota.reset_5h, reset_5h, now);
            } else if let Some(reset) = reset_5h {
                quota.reset_5h = Some(reset);
            }
            if wrote_utilization_5h {
                quota.observed_at_5h = Some(now);
            }
            if wrote_status_5h {
                quota.observed_at_status_5h = Some(now);
                quota.reset_at_status_5h =
                    reset_5h.or_else(|| quota.reset_5h.filter(|&reset| reset > now));
            }
            if wrote_utilization_7d || wrote_status_7d {
                quota.reset_7d = preserve_future_reset(quota.reset_7d, reset_7d, now);
            } else if let Some(reset) = reset_7d {
                quota.reset_7d = Some(reset);
            }
            if wrote_utilization_7d {
                quota.observed_at_7d = Some(now);
            }
            if wrote_status_7d {
                quota.observed_at_status_7d = Some(now);
                quota.reset_at_status_7d =
                    reset_7d.or_else(|| quota.reset_7d.filter(|&reset| reset > now));
            }
            if wrote_utilization_7d_oi || wrote_status_7d_oi {
                quota.reset_7d_oi = preserve_future_reset(quota.reset_7d_oi, reset_7d_oi, now);
            } else if let Some(reset) = reset_7d_oi {
                quota.reset_7d_oi = Some(reset);
            }
            if wrote_utilization_7d_oi {
                quota.observed_at_7d_oi = Some(now);
            }
            if wrote_status_7d_oi {
                quota.observed_at_status_7d_oi = Some(now);
                quota.reset_at_status_7d_oi =
                    reset_7d_oi.or_else(|| quota.reset_7d_oi.filter(|&reset| reset > now));
            }
            if wrote_status {
                quota.observed_at_status = Some(now);
            }

            // The post-lock dirty mark below covers both this observation and
            // any expiry found while recomputing the provider metric.
            let (utilization, _quota_expired) =
                self.pool_utilization_for(provider, &mut entries, now);
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
            let health = entries.entry(account_key(provider, account)).or_default();
            health.observed = true;
            let quota = &mut health.quota;
            let now = unix_now();

            expire_stale_quota(quota, now);

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
                let Some(window) = minutes.and_then(codex_window_bucket) else {
                    continue;
                };
                // The reset header can be blank (issue: a deployed multi-account
                // codex pool observed empty `x-codex-*-reset-at` groups), so
                // `reset` is best-effort while the utilization/status observation
                // below is unconditional — `observed_at_X` must not depend on
                // whether the reset happened to be present this time.
                let reset = header_value::<u64>(headers, reset_header);
                match window {
                    CodexWindow::FiveHour => {
                        if let Some(utilization) = utilization {
                            quota.utilization_5h = Some(utilization / 100.0);
                            quota.observed_at_5h = Some(now);
                            quota.reset_5h = preserve_future_reset(quota.reset_5h, reset, now);
                        } else if let Some(reset) = reset {
                            quota.reset_5h = Some(reset);
                        }
                    }
                    CodexWindow::Weekly => {
                        if let Some(utilization) = utilization {
                            quota.utilization_7d = Some(utilization / 100.0);
                            quota.observed_at_7d = Some(now);
                            quota.reset_7d = preserve_future_reset(quota.reset_7d, reset, now);
                        } else if let Some(reset) = reset {
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
                quota.observed_at_status = Some(now);
            }
            // The post-lock dirty mark below covers both this observation and
            // any expiry found while recomputing the provider metric.
            let (utilization, _quota_expired) =
                self.pool_utilization_for(provider, &mut entries, now);
            record_pool_utilization(provider, utilization);
        }
        self.mark_dirty();
    }

    /// Apply an authoritative usage snapshot from the Anthropic OAuth usage API
    /// to an account's quota state. Each reported window's utilization always
    /// overwrites the stored value — the usage API is authoritative and
    /// reconciles the header-derived state with out-of-band consumption. The
    /// window's `resets_at` overwrites the stored reset when present; when the
    /// poll omits it, the stored reset survives only if still in the future
    /// (see [`preserve_future_reset`]) — a past stored reset is cleared instead
    /// of kept, so it cannot suppress the utilization this same call just
    /// wrote at the next `expire_stale_quota` sweep. A window the snapshot
    /// omits entirely leaves any prior value (utilization, reset, observation
    /// time, and status metadata) untouched. Status fields and their freshness
    /// boundaries are not modified here: the usage API has no equivalent of
    /// the headers' `rejected` signals, so they stay header-driven. Marks the
    /// account observed, so the admin dashboard reports its usage even before
    /// the first proxied request.
    pub fn note_usage(&self, provider: &str, account: &AccountConfig, usage: &UsageSnapshot) {
        {
            let mut entries = self.entries.lock().expect("account health lock poisoned");
            let health = entries.entry(account_key(provider, account)).or_default();
            health.observed = true;
            let quota = &mut health.quota;
            let now = unix_now();
            expire_stale_quota(quota, now);
            if let Some(window) = &usage.five_hour {
                quota.utilization_5h = Some(window.utilization);
                quota.reset_5h = preserve_future_reset(quota.reset_5h, window.resets_at, now);
                quota.observed_at_5h = Some(now);
            }
            if let Some(window) = &usage.seven_day {
                quota.utilization_7d = Some(window.utilization);
                quota.reset_7d = preserve_future_reset(quota.reset_7d, window.resets_at, now);
                quota.observed_at_7d = Some(now);
            }
            if let Some(window) = &usage.seven_day_oi {
                quota.utilization_7d_oi = Some(window.utilization);
                quota.reset_7d_oi = preserve_future_reset(quota.reset_7d_oi, window.resets_at, now);
                quota.observed_at_7d_oi = Some(now);
            }
            // The post-lock dirty mark below covers both this observation and
            // any expiry found while recomputing the provider metric.
            let (utilization, _quota_expired) =
                self.pool_utilization_for(provider, &mut entries, now);
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
        self.cooldown_scoped(provider, account, duration, reason, CooldownScope::Account);
    }

    pub fn cooldown_scoped(
        &self,
        provider: &str,
        account: &AccountConfig,
        duration: Duration,
        reason: &'static str,
        scope: CooldownScope,
    ) {
        let mut entries = self.entries.lock().expect("account health lock poisoned");
        let health = entries.entry(account_key(provider, account)).or_default();
        health.observed = true;
        health.enabled = !account.disabled;
        match scope {
            CooldownScope::Account => health.cooldown_until = Some(Instant::now() + duration),
            CooldownScope::Fable => health.cooldown_until_fable = Some(Instant::now() + duration),
        }
        // Any failover-worthy failure restarts the storm-control ramp: when the
        // account comes back it re-enters slow start instead of inheriting the
        // allowance it had grown before failing.
        health.ramp_allowance = 0;
        drop(entries);
        crate::metrics::record_pool_rotation(provider, reason);
    }

    /// Record that this account's credential is terminally dead: only an
    /// operator re-login can revive it. Callers must have established that the
    /// failure is terminal — [`crate::auth::claude::auth::is_terminal_refresh_failure`]
    /// for a rejected refresh, or a 401 on an unrefreshable credential — never
    /// on a transient failure.
    ///
    /// Deliberately separate from [`Self::cooldown`]: this changes nothing
    /// about selection or the cooldown clock, it only makes the dead account
    /// visible to the operator instead of letting it cycle silently.
    pub fn mark_needs_relogin(&self, provider: &str, account: &AccountConfig, cause: ReloginCause) {
        let key = account_key(provider, account);
        let mut entries = self.entries.lock().expect("account health lock poisoned");
        for (sibling, health) in entries.iter_mut() {
            if backed_by_same_store_account(sibling, &key, account) {
                // `observed` too, and not only for symmetry with the entry
                // below: `snapshot` reports an unobserved entry as `unseen`
                // and drops every field on it, so a sibling that `select_order`
                // has created but no response has answered yet would carry the
                // mark and still render clean — the fan-out would be invisible
                // on exactly the row it was added for. The pool *has* processed
                // an upstream response for this credential; that is what the
                // fan-out asserts.
                health.observed = true;
                health.needs_relogin = Some(ReloginCause::strongest(health.needs_relogin, cause));
            }
        }
        let health = entries.entry(key).or_default();
        health.observed = true;
        health.enabled = !account.disabled;
        health.needs_relogin = Some(ReloginCause::strongest(health.needs_relogin, cause));
    }

    /// Clear the needs-re-login mark alone, leaving every other health field —
    /// the cooldown included — untouched. Used where a response proves the
    /// credential authenticated without proving the account is healthy: a
    /// relayed non-401 4xx after a refresh. [`Self::mark_healthy_scoped`] is
    /// the wrong tool there because it also clears the cooldown, and this
    /// change is meant to add a signal, not to alter routing.
    pub fn clear_needs_relogin(&self, provider: &str, account: &AccountConfig) {
        let key = account_key(provider, account);
        let mut entries = self.entries.lock().expect("account health lock poisoned");
        for (sibling, health) in entries.iter_mut() {
            if *sibling == key || backed_by_same_store_account(sibling, &key, account) {
                health.needs_relogin = None;
            }
        }
    }

    /// Set or clear the needs-re-login mark on every pool entry backed by one
    /// store account, whatever provider table it is reachable through. Used by
    /// the admin re-login and refresh-probe paths, which know an account by its
    /// store name rather than by the provider entry that selected it.
    ///
    /// Takes the name *and* the uuid rather than one conflated identity string,
    /// because the three [`AccountStateIdentity`] variants are keyed
    /// differently and a store account can land in any of them:
    ///
    /// - `Verified` — the credential file carried a `shuntAccountUuid`; keyed
    ///   by that uuid.
    /// - `StoreEntry` — a scanned store account with no uuid; keyed by name.
    /// - `UpstreamInline` — **the shape a name-only `[[providers.*.accounts]]`
    ///   entry gets**, which is the documented way to activate one store
    ///   account. `resolve_pool_accounts` leaves it `store_entry = false` with
    ///   no uuid (`inline_identity_key` returns `None` without a `credentials`
    ///   path or `token_env`), so it is keyed by `(upstream, name)`. Skipping
    ///   this variant made the admin probe unable to mark, and a re-login
    ///   unable to clear, exactly the accounts operators are told to configure.
    ///
    /// The two name-keyed variants are matched on the name alone, which is all
    /// [`AccountKey`] carries. An inline account that names a *different*
    /// credential (a `credentials` path whose file has no uuid) and happens to
    /// share this store account's name would therefore also match. Both error
    /// directions self-correct — a wrong set is cleared by that account's next
    /// success, a wrong clear is re-established by its next terminal failure —
    /// so this is preferred over leaving the ordinary configuration unreachable.
    ///
    /// Only entries the pool already holds are updated — an account the pool
    /// has never selected has no health entry, and the dashboard reports it as
    /// `unseen` rather than carrying any mark. Inventing an entry here would
    /// mean synthesizing an [`AccountKey`] the selection path never produced.
    pub fn set_needs_relogin_for_store_account(
        &self,
        store_family: StoreFamily,
        account_name: &str,
        account_uuid: Option<&str>,
        needs_relogin: bool,
    ) {
        let mut entries = self.entries.lock().expect("account health lock poisoned");
        for (key, health) in entries.iter_mut() {
            let matches = key.store_family == store_family
                && match &key.identity {
                    AccountStateIdentity::Verified { id } => account_uuid == Some(id.as_str()),
                    AccountStateIdentity::StoreEntry { name }
                    | AccountStateIdentity::UpstreamInline { name, .. } => name == account_name,
                };
            if matches {
                health.needs_relogin = needs_relogin.then(|| {
                    ReloginCause::strongest(health.needs_relogin, ReloginCause::RefreshGrant)
                });
            }
        }
    }

    /// Whether any pool entry backed by one store account still carries the
    /// needs-re-login mark. Matched the same way as
    /// [`Self::set_needs_relogin_for_store_account`], because the admin paths
    /// know an account by its store name and uuid rather than by the provider
    /// entry that selected it.
    ///
    /// The refresh probe reports this *after* its own clear, so its response
    /// cannot claim recovery for an account the pool still considers dead.
    pub fn store_account_needs_relogin(
        &self,
        store_family: StoreFamily,
        account_name: &str,
        account_uuid: Option<&str>,
    ) -> bool {
        let entries = self.entries.lock().expect("account health lock poisoned");
        entries.iter().any(|(key, health)| {
            health.needs_relogin.is_some()
                && key.store_family == store_family
                && match &key.identity {
                    AccountStateIdentity::Verified { id } => account_uuid == Some(id.as_str()),
                    AccountStateIdentity::StoreEntry { name }
                    | AccountStateIdentity::UpstreamInline { name, .. } => name == account_name,
                }
        })
    }

    /// Clear the mark on every pool entry backed by one store account, but only
    /// where a *grant* failure set it ([`ReloginCause::RefreshGrant`]).
    ///
    /// This is what the admin refresh probe calls on success. The probe only
    /// exercises the refresh grant, which proves the refresh token is alive —
    /// not that the account can serve inference. An account marked because the
    /// provider rejected a bearer it actually presented
    /// ([`ReloginCause::ServedRequest`]) is still dead, and clearing it here
    /// would hide that until the next real request re-established it. Such a
    /// mark is cleared only by a served response, in
    /// [`Self::mark_healthy_scoped`].
    pub fn clear_grant_relogin_for_store_account(
        &self,
        store_family: StoreFamily,
        account_name: &str,
        account_uuid: Option<&str>,
    ) {
        let mut entries = self.entries.lock().expect("account health lock poisoned");
        for (key, health) in entries.iter_mut() {
            if health.needs_relogin != Some(ReloginCause::RefreshGrant) {
                continue;
            }
            let matches = key.store_family == store_family
                && match &key.identity {
                    AccountStateIdentity::Verified { id } => account_uuid == Some(id.as_str()),
                    AccountStateIdentity::StoreEntry { name }
                    | AccountStateIdentity::UpstreamInline { name, .. } => name == account_name,
                };
            if matches {
                health.needs_relogin = None;
            }
        }
    }

    /// Whether this account currently carries the needs-re-login mark.
    pub fn needs_relogin(&self, provider: &str, account: &AccountConfig) -> bool {
        let entries = self.entries.lock().expect("account health lock poisoned");
        entries
            .get(&account_key(provider, account))
            .is_some_and(|health| health.needs_relogin.is_some())
    }

    /// Clear the account-wide cooldown and record the account as observed-healthy.
    /// A non-Fable success proves nothing about the Fable quota bucket, so this
    /// compatibility entry point deliberately leaves the Fable-only slot intact.
    /// `turn_succeeded` gates slow-start growth: a relayed client error (4xx)
    /// proves the account reachable — hence healthy — but must not pre-warm
    /// storm-control capacity, or a burst of malformed requests would bypass
    /// slow start before valid traffic arrives.
    pub fn mark_healthy(&self, provider: &str, account: &AccountConfig, turn_succeeded: bool) {
        self.mark_healthy_scoped(provider, account, turn_succeeded, false);
    }

    pub fn mark_healthy_scoped(
        &self,
        provider: &str,
        account: &AccountConfig,
        turn_succeeded: bool,
        is_fable: bool,
    ) {
        let key = account_key(provider, account);
        let mut entries = self.entries.lock().expect("account health lock poisoned");
        // A served response disproves the mark on every row backed by this
        // credential, not only the row that carried the request — the mark fans
        // out, so the clear has to reach at least as far or a live credential
        // stays condemned on its sibling rows until each one happens to serve a
        // request of its own. Only the mark: the cooldown and the ramp stay
        // per-row, because this change adds a signal and must not alter routing.
        for (sibling, health) in entries.iter_mut() {
            if *sibling != key && backed_by_same_store_account(sibling, &key, account) {
                health.needs_relogin = None;
            }
        }
        let health = entries.entry(key).or_default();
        health.observed = true;
        health.enabled = !account.disabled;
        health.cooldown_until = None;
        // A response the account actually served proves the credential is
        // alive, whatever a previous 401 concluded.
        health.needs_relogin = None;
        if is_fable {
            health.cooldown_until_fable = None;
        }
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
        let key = account_key(provider, account);
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

    /// Forget pool health and refresh state for one physical store identity.
    /// Removing persisted quota marks the pool dirty so the next flush removes it.
    pub fn forget_identity(&self, store_family: StoreFamily, identity: &str) {
        let matches = |key: &AccountKey| {
            key.store_family == store_family
                && match &key.identity {
                    AccountStateIdentity::Verified { id }
                    | AccountStateIdentity::StoreEntry { name: id } => id == identity,
                    AccountStateIdentity::UpstreamInline { .. } => false,
                }
        };
        let removed_quota = {
            let mut entries = self.entries.lock().expect("account health lock poisoned");
            let removed_quota = entries
                .iter()
                .any(|(key, health)| matches(key) && health.quota.has_signal());
            entries.retain(|key, _| !matches(key));
            removed_quota
        };
        self.refresh_locks
            .lock()
            .expect("account refresh-lock map poisoned")
            .retain(|key, _| !matches(key));
        // Purge the forgotten identity from every upstream's membership map too,
        // so a deleted/rotated account leaves no dead `AccountKey` behind (the
        // per-upstream map is otherwise only rebuilt on the next
        // `sync_enabled_accounts`, which never runs for a decommissioned
        // upstream). Empty inner maps are left in place — harmless, and reused on
        // the next sync.
        self.memberships
            .lock()
            .expect("account membership lock poisoned")
            .values_mut()
            .for_each(|members| members.retain(|key, _| !matches(key)));
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
        let (snapshots, quota_expired) = {
            let mut entries = self.entries.lock().expect("account health lock poisoned");
            let mut quota_expired = false;
            let snapshots = accounts
                .iter()
                .map(|account| {
                    let key = account_key(provider, account);
                    let Some(health) = entries.get_mut(&key).filter(|health| health.observed)
                    else {
                        // Never selected, or selected but not yet answered (a default
                        // entry from `select_order`): report a clean, available slot.
                        return AccountSnapshot::unseen(account);
                    };
                    quota_expired |= expire_stale_quota(&mut health.quota, unix_now);
                    let quota = assess_quota(&health.quota, account, is_fable, pool, unix_now);
                    let cooldown_secs_remaining = health
                        .cooldown_until
                        .and_then(|until| until.checked_duration_since(now))
                        .map(|remaining| remaining.as_secs());
                    let cooldown_fable_secs_remaining = health
                        .cooldown_until_fable
                        .and_then(|until| until.checked_duration_since(now))
                        .map(|remaining| remaining.as_secs());
                    let cooling = cooldown_secs_remaining.is_some()
                        || (is_fable && cooldown_fable_secs_remaining.is_some());
                    AccountSnapshot {
                        name: account.name.clone(),
                        has_state: true,
                        available: !account.disabled && !cooling && !quota.near,
                        near_quota: quota.near,
                        cooldown_secs_remaining,
                        cooldown_fable_secs_remaining,
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
                        needs_relogin: health.needs_relogin.is_some(),
                    }
                })
                .collect();
            (snapshots, quota_expired)
        };
        if quota_expired {
            self.mark_dirty();
        }
        snapshots
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

    /// Return an account's raw quota without running any expiry or persistence
    /// path. This is test-only so restore tests can inspect state before a
    /// later selection, snapshot, or export sweep.
    #[cfg(test)]
    pub(crate) fn raw_quota_for_test(&self, key: &AccountKey) -> Option<(bool, QuotaState)> {
        let entries = self.entries.lock().expect("account health lock poisoned");
        entries
            .get(key)
            .map(|health| (health.observed, health.quota.clone()))
    }

    /// Snapshot every observed physical account's quota for on-disk persistence.
    pub(crate) fn export_quotas(&self) -> Vec<(AccountKey, QuotaState)> {
        let (quotas, quota_expired) = {
            let mut entries = self.entries.lock().expect("account health lock poisoned");
            let now = unix_now();
            let mut quota_expired = false;
            let quotas = entries
                .iter_mut()
                .filter_map(|(key, health)| {
                    quota_expired |= expire_stale_quota(&mut health.quota, now);
                    (health.observed && health.quota.has_signal())
                        .then(|| (key.clone(), health.quota.clone()))
                })
                .collect();
            (quotas, quota_expired)
        };
        if quota_expired {
            self.mark_dirty();
        }
        quotas
    }

    /// Seed the pool with quotas restored from disk at boot. Returns whether
    /// any quota or observation timestamp was corrected. Expired quota is
    /// swept before missing timestamps are backfilled, so a past-reset window
    /// cannot be made fresh during migration. A restored signal with no
    /// observation time is stamped with boot time rather than left unstamped:
    /// `expire_stale_quota` treats an unstamped reset-less signal as expired,
    /// which would defeat the intended warm start. Version-2 migration uses
    /// the old combined timestamp for surviving per-window status and may
    /// synthesize an aggregate deadline stamp from the earliest captured
    /// reset. That synthetic value can remain in the v3 rewrite; normal v3
    /// import does not infer either form from reset metadata. Every import
    /// still normalizes orphan metadata, expires elapsed signals, clamps
    /// future timestamps to boot time, and stamps a surviving aggregate that
    /// lacks its observation time with boot time. This correction runs only at
    /// import, so a future timestamp created at runtime by a backwards clock
    /// remains until the next restart.
    pub(crate) fn import_quotas(
        &self,
        quotas: impl IntoIterator<Item = (AccountKey, QuotaState)>,
    ) -> bool {
        self.import_quotas_mode(quotas, false)
    }

    /// Import quota state with the version-2 combined timestamp compatibility
    /// path. Only persistence migration may enable this mode; live callers and
    /// normal v3 restore must keep utilization and status freshness separate.
    /// The legacy path may synthesize an aggregate deadline stamp from the
    /// earliest captured reset. That value is retained by the v3 rewrite;
    /// normal v3 restore does not reinterpret it from reset metadata.
    pub(crate) fn import_quotas_legacy(
        &self,
        quotas: impl IntoIterator<Item = (AccountKey, QuotaState)>,
    ) -> bool {
        self.import_quotas_mode(quotas, true)
    }

    fn import_quotas_mode(
        &self,
        quotas: impl IntoIterator<Item = (AccountKey, QuotaState)>,
        legacy_combined_timestamps: bool,
    ) -> bool {
        let mut entries = self.entries.lock().expect("account health lock poisoned");
        let now = unix_now();
        let mut corrected = false;
        for (key, mut quota) in quotas {
            // v2's combined timestamp belongs to whichever signal survived;
            // normalize that ownership before expiry so a stale utilization
            // timestamp cannot make a future reset-only record disappear.
            // The same legacy pass captures an unstamped aggregate's earliest
            // reset deadline before `stamp_missing_observation` can apply the
            // ordinary boot-time fallback.
            if legacy_combined_timestamps {
                corrected |= migrate_legacy_status_timestamps(&mut quota, now);
            }
            corrected |= normalize_signal_metadata(&mut quota);
            corrected |= expire_stale_quota(&mut quota, now);
            corrected |= stamp_missing_observation(&mut quota, now);
            corrected |= clamp_future_observation(&mut quota.observed_at_5h, now);
            corrected |= clamp_future_observation(&mut quota.observed_at_7d, now);
            corrected |= clamp_future_observation(&mut quota.observed_at_7d_oi, now);
            corrected |= clamp_future_observation(&mut quota.observed_at_status_5h, now);
            corrected |= clamp_future_observation(&mut quota.observed_at_status_7d, now);
            corrected |= clamp_future_observation(&mut quota.observed_at_status_7d_oi, now);
            corrected |= clamp_future_observation(&mut quota.observed_at_status, now);
            let health = entries.entry(key).or_default();
            health.observed = true;
            health.quota = quota;
        }
        corrected
    }

    fn pool_utilization_for(
        &self,
        upstream: &str,
        entries: &mut HashMap<AccountKey, AccountHealth>,
        now: u64,
    ) -> ([Option<f64>; 3], bool) {
        let memberships = self
            .memberships
            .lock()
            .expect("account membership lock poisoned");
        let Some(members) = memberships.get(upstream) else {
            return ([None; 3], false);
        };
        let mut minimums = [None::<f64>; 3];
        let mut quota_expired = false;
        for (key, enabled) in members {
            if !enabled {
                continue;
            }
            let Some(health) = entries.get_mut(key) else {
                continue;
            };
            quota_expired |= expire_stale_quota(&mut health.quota, now);
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
        (minimums, quota_expired)
    }

    fn cancel_reprobe_token(&self, key: &AccountKey, token: u64) {
        let mut entries = self.entries.lock().expect("account health lock poisoned");
        if let Some(health) = entries.get_mut(key) {
            if health.reprobe_reservation == Some(token) {
                health.reprobe_reservation = None;
            }
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
                .entry(account_key(provider, account))
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

/// Whether `sibling` is another pool entry for the very same store account as
/// the one `key`/`account` describe — the fan-out predicate shared by
/// [`AccountPool::mark_needs_relogin`] and [`AccountPool::clear_needs_relogin`].
///
/// A store account activated by name in two `[[providers.*]]` tables gets a
/// **separate** health entry per table, because `resolve_pool_accounts` leaves
/// such an entry UUID-less (`inline_identity_key` returns `None` without a
/// `credentials` path) and [`account_key`] then keys it as `UpstreamInline`,
/// which carries the upstream name. One credential file, several keys. Marking
/// only the entry that happened to see the terminal failure leaves its siblings
/// reporting `available` and retrying the same dead credential — the loop this
/// mark exists to end.
///
/// The fan-out is deliberately narrow in two ways:
///
/// - Only an account that *is* the store account fans out. One carrying its own
///   `credentials` path or `token_env` merely shares a name with it and is a
///   different credential, so a failure on it says nothing about the store file.
/// - A sibling keyed as `Verified` is reached only when this account also
///   carries that uuid. A name-only entry has none, so it cannot reach a
///   uuid-keyed entry for the same store file — [`AccountKey`] keeps no name
///   there to match on. That residue is the pre-existing per-key behaviour, not
///   a regression, and the uuid-keyed entry still self-corrects on its own next
///   terminal failure.
fn backed_by_same_store_account(
    sibling: &AccountKey,
    key: &AccountKey,
    account: &AccountConfig,
) -> bool {
    if account.credentials.is_some() || account.token_env.is_some() {
        return false;
    }
    sibling.store_family == key.store_family
        && match &sibling.identity {
            AccountStateIdentity::Verified { id } => account
                .uuid
                .as_deref()
                .is_some_and(|uuid| !uuid.trim().is_empty() && uuid == id),
            AccountStateIdentity::StoreEntry { name }
            | AccountStateIdentity::UpstreamInline { name, .. } => *name == account.name,
        }
}

pub(crate) fn account_key(upstream: &str, account: &AccountConfig) -> AccountKey {
    let store_family = account.store_family.unwrap_or_else(|| {
        // Only a caller that bypassed `resolve_pool_accounts` (which always
        // stamps the family) lands here, so this is a defensive guess, not the
        // normal path. Case-insensitive so an upstream named e.g. `My-Codex`
        // still infers the ChatGPT store family instead of falling through to
        // Claude. Kimi is matched before the Claude fallback because it would
        // otherwise be keyed as a Claude account and could collide with a
        // real Claude account of the same name — note `kimi-code`, the
        // preset's own name, does *not* contain `codex`.
        let lower = upstream.to_lowercase();
        if lower.contains("codex") || lower.contains("chatgpt") {
            StoreFamily::Chatgpt
        } else if lower.contains("kimi") {
            StoreFamily::Kimi
        } else {
            StoreFamily::Claude
        }
    });
    let identity = match account.uuid.as_deref().filter(|id| !id.trim().is_empty()) {
        Some(id) => AccountStateIdentity::Verified { id: id.to_string() },
        None if account.store_entry => AccountStateIdentity::StoreEntry {
            name: account.name.clone(),
        },
        None => AccountStateIdentity::UpstreamInline {
            upstream: upstream.to_string(),
            name: account.name.clone(),
        },
    };
    AccountKey {
        store_family,
        identity,
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
fn collapse_representatives(upstream: &str, accounts: &[AccountConfig]) -> Vec<usize> {
    let mut slots = HashMap::<AccountKey, usize>::with_capacity(accounts.len());
    let mut representatives: Vec<usize> = Vec::with_capacity(accounts.len());
    for (index, account) in accounts.iter().enumerate() {
        let identity = account_key(upstream, account);
        if let Some(&slot) = slots.get(&identity) {
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

/// Records the header's value into `field` when present. Returns whether a
/// value was recorded, so callers can stamp a window's observation time only
/// when this call actually wrote something.
fn update_header<T: std::str::FromStr>(
    headers: &HeaderMap,
    name: &str,
    field: &mut Option<T>,
) -> bool {
    if let Some(parsed) = header_value(headers, name) {
        *field = Some(parsed);
        true
    } else {
        false
    }
}

/// String counterpart of [`update_header`]; same observed-write contract.
fn update_string_header(headers: &HeaderMap, name: &str, field: &mut Option<String>) -> bool {
    if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) {
        *field = Some(value.to_string());
        true
    } else {
        false
    }
}

/// Reset to store for a usage-API window: the poll's own `resets_at` wins
/// outright when present; otherwise the previously stored reset survives
/// only if it is still in the future. A past stored reset is cleared rather
/// than kept, because keeping it would let the next `expire_stale_quota`
/// sweep erase the utilization this same call just wrote, and the next poll
/// would write it again — an indefinite write/expire/rewrite cycle. Once
/// cleared, `observed_at_X` alone governs this window's expiry.
fn preserve_future_reset(stored: Option<u64>, polled: Option<u64>, now: u64) -> Option<u64> {
    polled.or_else(|| stored.filter(|&reset| reset > now))
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

pub fn is_fable_model(model: Option<&str>) -> bool {
    model.is_some_and(|model| model.to_ascii_lowercase().contains("fable"))
}

fn governing_cooldown(health: &AccountHealth, is_fable: bool) -> Option<Instant> {
    // Fable traffic must wait for both applicable cooldowns, so the later expiry governs.
    if is_fable {
        match (health.cooldown_until, health.cooldown_until_fable) {
            (Some(account), Some(fable)) => Some(account.max(fable)),
            (account, fable) => account.or(fable),
        }
    } else {
        health.cooldown_until
    }
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
    let weekly = if is_fable && quota.utilization_7d_oi.is_some() {
        (
            quota.utilization_7d_oi,
            quota.reset_7d_oi,
            QuotaWindow::Fable,
        )
    } else {
        (quota.utilization_7d, quota.reset_7d, QuotaWindow::Weekly)
    };
    let has_window_status =
        quota.status_5h.is_some() || quota.status_7d.is_some() || quota.status_7d_oi.is_some();
    // Rejection statuses arrive independently of utilization, so they cannot use
    // the utilization-presence gate that selects the threshold/headroom window.
    // A shared weekly rejection also blocks Fable requests; only an isolated
    // `7d_oi` rejection is Fable-specific.
    let weekly_rejected = quota.status_7d.as_deref() == Some("rejected")
        || (is_fable && quota.status_7d_oi.as_deref() == Some("rejected"));
    let rejected = quota.status_5h.as_deref() == Some("rejected")
        || weekly_rejected
        || (!has_window_status && quota.status.as_deref() == Some("rejected"));
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

fn record_pool_utilization(provider: &str, utilization: [Option<f64>; 3]) {
    for (window, value) in ["5h", "7d", "7d_oi"].into_iter().zip(utilization) {
        crate::metrics::record_pool_utilization(provider, window, value);
    }
}

/// Clears each signal once its own reset or timestamp lifetime expires. A
/// stamped aggregate has its own unconditional cap and is cleared only when
/// that cap expires. A reset-less signal cannot outlive its window length.
fn expire_stale_quota(quota: &mut QuotaState, now: u64) -> bool {
    let mut expired = false;
    let reset_expired = |reset: Option<u64>| reset.is_some_and(|reset| reset <= now);
    let observation_cap_expired =
        |observed: Option<u64>, len: u64| observed.is_some_and(|at| at.saturating_add(len) <= now);
    if reset_expired(quota.reset_5h) {
        let had_signal = quota.utilization_5h.is_some()
            || quota.reset_5h.is_some()
            || quota.observed_at_5h.is_some();
        quota.utilization_5h = None;
        quota.reset_5h = None;
        quota.observed_at_5h = None;
        expired |= had_signal;
    } else if observation_cap_expired(quota.observed_at_5h, WINDOW_5H_SECS) {
        let had_signal = quota.utilization_5h.is_some() || quota.observed_at_5h.is_some();
        quota.utilization_5h = None;
        quota.observed_at_5h = None;
        expired |= had_signal;
    }
    if reset_expired(quota.reset_at_status_5h)
        || observation_cap_expired(quota.observed_at_status_5h, WINDOW_5H_SECS)
    {
        let had_signal = quota.status_5h.is_some()
            || quota.reset_at_status_5h.is_some()
            || quota.observed_at_status_5h.is_some();
        quota.status_5h = None;
        quota.reset_at_status_5h = None;
        quota.observed_at_status_5h = None;
        expired |= had_signal;
    }
    if reset_expired(quota.reset_7d) {
        let had_signal = quota.utilization_7d.is_some()
            || quota.reset_7d.is_some()
            || quota.observed_at_7d.is_some();
        quota.utilization_7d = None;
        quota.reset_7d = None;
        quota.observed_at_7d = None;
        expired |= had_signal;
    } else if observation_cap_expired(quota.observed_at_7d, WINDOW_7D_SECS) {
        let had_signal = quota.utilization_7d.is_some() || quota.observed_at_7d.is_some();
        quota.utilization_7d = None;
        quota.observed_at_7d = None;
        expired |= had_signal;
    }
    if reset_expired(quota.reset_at_status_7d)
        || observation_cap_expired(quota.observed_at_status_7d, WINDOW_7D_SECS)
    {
        let had_signal = quota.status_7d.is_some()
            || quota.reset_at_status_7d.is_some()
            || quota.observed_at_status_7d.is_some();
        quota.status_7d = None;
        quota.reset_at_status_7d = None;
        quota.observed_at_status_7d = None;
        expired |= had_signal;
    }
    if reset_expired(quota.reset_7d_oi) {
        let had_signal = quota.utilization_7d_oi.is_some()
            || quota.reset_7d_oi.is_some()
            || quota.observed_at_7d_oi.is_some();
        quota.utilization_7d_oi = None;
        quota.reset_7d_oi = None;
        quota.observed_at_7d_oi = None;
        expired |= had_signal;
    } else if observation_cap_expired(quota.observed_at_7d_oi, WINDOW_7D_SECS) {
        let had_signal = quota.utilization_7d_oi.is_some() || quota.observed_at_7d_oi.is_some();
        quota.utilization_7d_oi = None;
        quota.observed_at_7d_oi = None;
        expired |= had_signal;
    }
    if reset_expired(quota.reset_at_status_7d_oi)
        || observation_cap_expired(quota.observed_at_status_7d_oi, WINDOW_7D_SECS)
    {
        let had_signal = quota.status_7d_oi.is_some()
            || quota.reset_at_status_7d_oi.is_some()
            || quota.observed_at_status_7d_oi.is_some();
        quota.status_7d_oi = None;
        quota.reset_at_status_7d_oi = None;
        quota.observed_at_status_7d_oi = None;
        expired |= had_signal;
    }
    // Legacy aggregate statuses have no independent observation time, so any
    // expired window remains their only expiry signal. Stamped aggregates are
    // governed solely by the unconditional cap below and survive it.
    if expired && quota.observed_at_status.is_none() {
        let had_signal = quota.status.is_some() || quota.observed_at_status.is_some();
        quota.status = None;
        quota.observed_at_status = None;
        expired |= had_signal;
    }
    // Unconditional aggregate cap, independent of the per-window sweep above.
    // `assess_quota`'s `has_window_status` fallback reads the aggregate
    // `status` only when no per-window status is present, so an
    // aggregate-only rejection needs its own lifetime bound regardless of
    // whether a window signal is still alive. Without this, a window kept
    // fresh by something that never touches `status` (e.g. a usage poller)
    // would leave a stale aggregate rejection with no expiry path at all —
    // the poller writes utilization/reset every cycle, so the per-window
    // sweep above never fires, and `status` would never clear on its own.
    if quota
        .observed_at_status
        .is_some_and(|at| at.saturating_add(WINDOW_7D_SECS) <= now)
    {
        let had_signal = quota.status.is_some() || quota.observed_at_status.is_some();
        quota.status = None;
        quota.observed_at_status = None;
        expired |= had_signal;
    }
    expired
}

/// Stamps a restored signal that has no observation time with boot time. This
/// is only a warm-start fallback; v2's aggregate migration runs first so an
/// encoded reset deadline is not replaced, and normal v3 import never copies
/// utilization freshness or reset metadata into a status field.
fn stamp_missing_observation(quota: &mut QuotaState, now: u64) -> bool {
    let mut corrected = false;
    if quota.utilization_5h.is_some() && quota.observed_at_5h.is_none() {
        quota.observed_at_5h = Some(now);
        corrected = true;
    }
    if quota.utilization_7d.is_some() && quota.observed_at_7d.is_none() {
        quota.observed_at_7d = Some(now);
        corrected = true;
    }
    if quota.utilization_7d_oi.is_some() && quota.observed_at_7d_oi.is_none() {
        quota.observed_at_7d_oi = Some(now);
        corrected = true;
    }
    if quota.status_5h.is_some() && quota.observed_at_status_5h.is_none() {
        quota.observed_at_status_5h = Some(now);
        corrected = true;
    }
    if quota.status_7d.is_some() && quota.observed_at_status_7d.is_none() {
        quota.observed_at_status_7d = Some(now);
        corrected = true;
    }
    if quota.status_7d_oi.is_some() && quota.observed_at_status_7d_oi.is_none() {
        quota.observed_at_status_7d_oi = Some(now);
        corrected = true;
    }
    if quota.status.is_some() && quota.observed_at_status.is_none() {
        quota.observed_at_status = Some(now);
        corrected = true;
    }
    corrected
}

/// Migrate v2's combined per-window timestamp into independent status
/// timestamps. A per-window status that survived the pre-backfill expiry
/// sweep captures the reset that was current when the old state was written;
/// an unstamped aggregate status instead synthesizes a stamp that encodes the
/// earliest reset across all windows. The synthesized value preserves the old
/// reset-driven deadline across the v3 rewrite and later reset-only or usage
/// updates. Normal v3 import must preserve that value and never repeat either
/// inference merely because reset metadata is present.
fn migrate_legacy_status_timestamps(quota: &mut QuotaState, now: u64) -> bool {
    let mut corrected = false;
    for (status, observed_utilization, observed_status, captured_reset, shared_reset) in [
        (
            &quota.status_5h,
            &mut quota.observed_at_5h,
            &mut quota.observed_at_status_5h,
            &mut quota.reset_at_status_5h,
            &quota.reset_5h,
        ),
        (
            &quota.status_7d,
            &mut quota.observed_at_7d,
            &mut quota.observed_at_status_7d,
            &mut quota.reset_at_status_7d,
            &quota.reset_7d,
        ),
        (
            &quota.status_7d_oi,
            &mut quota.observed_at_7d_oi,
            &mut quota.observed_at_status_7d_oi,
            &mut quota.reset_at_status_7d_oi,
            &quota.reset_7d_oi,
        ),
    ] {
        if status.is_some() && observed_status.is_none() {
            *observed_status = observed_utilization.or(Some(now));
            *captured_reset = *shared_reset;
            corrected = true;
        }
    }
    if quota.status.is_some() && quota.observed_at_status.is_none() {
        let earliest_reset = [quota.reset_5h, quota.reset_7d, quota.reset_7d_oi]
            .into_iter()
            .flatten()
            .min();
        quota.observed_at_status = Some(
            earliest_reset
                .map(|reset| reset.saturating_sub(WINDOW_7D_SECS).min(now))
                .unwrap_or(now),
        );
        corrected = true;
    }
    corrected
}

/// Remove observation metadata whose owned signal is absent. This keeps the
/// persisted representation honest and prevents metadata-only records from
/// being treated as warm state after a restart.
fn normalize_signal_metadata(quota: &mut QuotaState) -> bool {
    let mut corrected = false;
    if quota.utilization_5h.is_none() && quota.observed_at_5h.take().is_some() {
        corrected = true;
    }
    if quota.utilization_7d.is_none() && quota.observed_at_7d.take().is_some() {
        corrected = true;
    }
    if quota.utilization_7d_oi.is_none() && quota.observed_at_7d_oi.take().is_some() {
        corrected = true;
    }
    if quota.status_5h.is_none() {
        corrected |= quota.observed_at_status_5h.take().is_some();
        corrected |= quota.reset_at_status_5h.take().is_some();
    }
    if quota.status_7d.is_none() {
        corrected |= quota.observed_at_status_7d.take().is_some();
        corrected |= quota.reset_at_status_7d.take().is_some();
    }
    if quota.status_7d_oi.is_none() {
        corrected |= quota.observed_at_status_7d_oi.take().is_some();
        corrected |= quota.reset_at_status_7d_oi.take().is_some();
    }
    if quota.status.is_none() && quota.observed_at_status.take().is_some() {
        corrected = true;
    }
    corrected
}

fn clamp_future_observation(observed_at: &mut Option<u64>, now: u64) -> bool {
    if observed_at.is_some_and(|at| at > now) {
        *observed_at = Some(now);
        return true;
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CooldownScope {
    Account,
    Fable,
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

/// True when only the Fable weekly quota status rejects the response. A 5-hour
/// or shared-weekly rejection makes the failure account-wide instead.
pub fn is_fable_scoped_rejection(headers: &HeaderMap) -> bool {
    let rejected = |name: &str| headers.get(name).is_some_and(|value| value == "rejected");
    rejected(QUOTA_STATUS_HEADERS[2])
        && !rejected(QUOTA_STATUS_HEADERS[0])
        && !rejected(QUOTA_STATUS_HEADERS[1])
}

/// Low-cardinality pool-rotation reason for an upstream response that moves off
/// an account. A quota-rejected Anthropic 429 is distinguished from ordinary
/// throttling; 5xx and 401 retain their operational categories. A Kimi 402
/// (inactive subscription membership, routed here via `classify_kimi`) gets
/// its own `"membership"` label instead of falling into `"other"`, since it
/// is a distinct, persistent, account-level failure mode worth tracking
/// separately from transient ones.
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
    } else if status == StatusCode::PAYMENT_REQUIRED {
        "membership"
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

/// Classify a Kimi Code OAuth upstream response for account-pool failover.
/// Takes the same `(status, headers)` shape as [`classify`], with one added
/// case: `402 Payment Required` rotates instead of relaying. A Kimi account
/// whose subscription membership is inactive returns 402 on *every*
/// inference request — measured against a live account on 2026-08-18 — so
/// it is an account-specific, persistent failure, not a transient one a
/// client retry could work around. Left under `classify`'s generic `Relay`
/// fallthrough, it would both surface the 402 straight to the client instead
/// of trying the next account, and clear the account's cooldown via
/// `mark_healthy` (which treats any answered request as healthy), so a
/// permanently dead account would keep getting reselected.
pub fn classify_kimi(status: StatusCode, headers: &HeaderMap) -> FailoverAction {
    if status == StatusCode::PAYMENT_REQUIRED {
        return FailoverAction::Rotate;
    }
    classify(status, headers)
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
    use std::{collections::HashSet, sync::Arc};

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

    /// `ServedRequest` outranks `RefreshGrant`, and a later grant failure must
    /// not rewrite it. Otherwise a terminal admin probe on an account the proxy
    /// had already condemned would downgrade the verdict, and the next grant
    /// that happened to succeed would clear a mark nothing had disproved.
    #[test]
    fn a_grant_failure_never_downgrades_a_served_request_verdict() {
        let pool = AccountPool::new();
        let account = account_with_uuid("dead", "acct-dead");

        // The proxy proved a refreshed bearer still gets a 401.
        pool.mark_needs_relogin("anthropic", &account, ReloginCause::ServedRequest);
        // A terminal admin probe then reports its own (weaker) grant failure.
        pool.set_needs_relogin_for_store_account(
            StoreFamily::Claude,
            "dead",
            Some("acct-dead"),
            true,
        );
        // …and a later grant succeeds. It must not clear the stronger verdict.
        pool.clear_grant_relogin_for_store_account(StoreFamily::Claude, "dead", Some("acct-dead"));

        assert!(
            pool.needs_relogin("anthropic", &account),
            "a successful grant must not clear a mark the proxy set from a \
             rejected bearer, even after a grant failure was recorded on top"
        );

        // The mirror: a grant failure recorded on a *clean* entry is
        // grant-caused, so the same successful grant does clear it.
        let fresh = account_with_uuid("stale", "acct-stale");
        pool.mark_healthy("anthropic", &fresh, true);
        pool.set_needs_relogin_for_store_account(
            StoreFamily::Claude,
            "stale",
            Some("acct-stale"),
            true,
        );
        assert!(pool.needs_relogin("anthropic", &fresh));
        pool.clear_grant_relogin_for_store_account(
            StoreFamily::Claude,
            "stale",
            Some("acct-stale"),
        );
        assert!(
            !pool.needs_relogin("anthropic", &fresh),
            "a purely grant-caused mark must still be cleared by a successful grant"
        );
    }

    /// `clear_needs_relogin` is the narrow clear used where a response proves
    /// the credential authenticated without proving the account is healthy — a
    /// relayed non-401 4xx after a refresh. It must drop the mark and leave the
    /// cooldown standing; [`AccountPool::mark_healthy_scoped`] would drop both,
    /// which would turn a signal-only change into a routing change.
    #[test]
    fn clearing_the_mark_alone_leaves_the_cooldown_standing() {
        let pool = AccountPool::new();
        let account = account("dead");
        pool.cooldown("anthropic", &account, Duration::from_secs(300), "auth");
        pool.mark_needs_relogin("anthropic", &account, ReloginCause::ServedRequest);

        let before = pool.snapshot("anthropic", std::slice::from_ref(&account), None, None);
        assert!(before[0].needs_relogin);
        assert!(before[0].cooldown_secs_remaining.is_some());

        pool.clear_needs_relogin("anthropic", &account);

        let after = pool.snapshot("anthropic", std::slice::from_ref(&account), None, None);
        assert!(!after[0].needs_relogin, "the mark must be cleared");
        assert!(
            after[0].cooldown_secs_remaining.is_some(),
            "the cooldown must survive: this clear adds a signal, it does not \
             alter routing"
        );

        // The contrast that gives the assertion above its meaning: the healthy
        // mark drops both, which is why it is the wrong tool for that branch.
        pool.mark_needs_relogin("anthropic", &account, ReloginCause::ServedRequest);
        pool.mark_healthy_scoped("anthropic", &account, false, false);
        let healthy = pool.snapshot("anthropic", std::slice::from_ref(&account), None, None);
        assert!(!healthy[0].needs_relogin);
        assert!(healthy[0].cooldown_secs_remaining.is_none());
    }

    /// The three [`AccountStateIdentity`] shapes are keyed differently, and the
    /// admin paths only know a store account by name + uuid. A name-only
    /// `[[providers.*.accounts]]` entry — the documented way to activate a
    /// store account — becomes `UpstreamInline`, so skipping that variant would
    /// leave the ordinary configuration unmarkable and unclearable.
    #[test]
    fn the_store_account_setter_reaches_every_identity_shape() {
        let pool = AccountPool::new();
        // `Verified`: credential file carried a uuid.
        let verified = account_with_uuid("dead", "acct-dead");
        // `StoreEntry`: scanned store account, no uuid.
        let store_entry = AccountConfig {
            name: "dead".to_string(),
            store_entry: true,
            ..Default::default()
        };
        // `UpstreamInline`: name-only provider entry, no uuid, not a scan.
        let inline = account("dead");
        // Same name, different store family — must not be touched.
        let other_family = AccountConfig {
            name: "dead".to_string(),
            store_family: Some(StoreFamily::Chatgpt),
            ..Default::default()
        };
        // Different name — must not be touched.
        let bystander = account("alive");

        for account in [&verified, &store_entry, &inline, &other_family, &bystander] {
            pool.mark_healthy("anthropic", account, true);
        }

        pool.set_needs_relogin_for_store_account(
            StoreFamily::Claude,
            "dead",
            Some("acct-dead"),
            true,
        );

        assert!(
            pool.needs_relogin("anthropic", &verified),
            "a uuid-keyed (Verified) account was not marked"
        );
        assert!(
            pool.needs_relogin("anthropic", &store_entry),
            "a scanned (StoreEntry) account was not marked"
        );
        assert!(
            pool.needs_relogin("anthropic", &inline),
            "a name-only provider entry (UpstreamInline) was not marked — this is \
             the shape operators are told to configure"
        );
        assert!(
            !pool.needs_relogin("anthropic", &other_family),
            "a same-named account in another store family was marked"
        );
        assert!(
            !pool.needs_relogin("anthropic", &bystander),
            "an unrelated account was marked"
        );

        pool.set_needs_relogin_for_store_account(
            StoreFamily::Claude,
            "dead",
            Some("acct-dead"),
            false,
        );

        for (account, shape) in [
            (&verified, "Verified"),
            (&store_entry, "StoreEntry"),
            (&inline, "UpstreamInline"),
        ] {
            assert!(
                !pool.needs_relogin("anthropic", account),
                "a re-login did not clear the mark on the {shape} account"
            );
        }
    }

    /// One store account activated by name in two provider tables gets two
    /// health entries, because `UpstreamInline` carries the upstream name. They
    /// are backed by one credential file, so a terminal verdict on either is a
    /// verdict on both — marking only the row that happened to serve the failing
    /// request leaves the other reporting `available` and retrying the same dead
    /// credential, which is the loop this mark exists to end.
    #[test]
    fn a_terminal_failure_marks_every_provider_row_backed_by_one_store_account() {
        let pool = AccountPool::new();
        let shared = account("work");
        let bystander = account("spare");

        // Both provider tables have selected the account at least once, so both
        // health entries exist before anything fails.
        for provider in ["claude-a", "claude-b"] {
            pool.mark_healthy(provider, &shared, true);
            pool.mark_healthy(provider, &bystander, true);
        }

        pool.mark_needs_relogin("claude-a", &shared, ReloginCause::RefreshGrant);

        assert!(
            pool.needs_relogin("claude-a", &shared),
            "the row that saw the failure was not marked"
        );
        assert!(
            pool.needs_relogin("claude-b", &shared),
            "the sibling row for the same store account was left unmarked, so it \
             keeps retrying the dead credential and reports `available`"
        );
        assert!(
            !pool.needs_relogin("claude-b", &bystander),
            "an unrelated store account was marked"
        );
    }

    /// The fan-out's negative twin, and the reason it is keyed on more than the
    /// name: an account carrying its own `credentials` path is a *different*
    /// credential that merely shares a name with a store account. A failure on
    /// it says nothing about the store file, so it must not condemn it.
    #[test]
    fn a_failure_on_an_inline_credential_never_marks_a_same_named_store_account() {
        let pool = AccountPool::new();
        let store_backed = account("work");
        let own_file = AccountConfig {
            name: "work".to_string(),
            credentials: Some("/tmp/somewhere-else.json".to_string()),
            ..Default::default()
        };
        pool.mark_healthy("claude-a", &store_backed, true);
        pool.mark_healthy("claude-b", &own_file, true);

        pool.mark_needs_relogin("claude-b", &own_file, ReloginCause::ServedRequest);

        assert!(
            pool.needs_relogin("claude-b", &own_file),
            "the account that actually failed was not marked"
        );
        assert!(
            !pool.needs_relogin("claude-a", &store_backed),
            "a same-named store account was condemned by a failure on an unrelated \
             credential file"
        );
    }

    /// The fan-out has to survive the snapshot, not just the map. `snapshot`
    /// drops every field of an entry whose `observed` is false and reports it as
    /// `unseen`, and `select_order` creates exactly such an entry for a row it
    /// has picked but that has not answered yet — the row most likely to be a
    /// sibling. Without stamping `observed`, the mark would be set and still
    /// render clean on the dashboard.
    #[test]
    fn a_fanned_out_mark_is_visible_on_a_sibling_the_pool_has_only_selected() {
        let pool = AccountPool::new();
        let accounts = vec![account("work")];
        // `claude-b` has selected the account — a default, unobserved entry —
        // but no response has come back through it.
        pool.select_order("claude-b", &accounts, Some("session"), None, None);
        assert!(
            !pool.snapshot("claude-b", &accounts, None, None)[0].has_state,
            "precondition: the sibling row is unobserved, so it renders as unseen"
        );

        pool.mark_needs_relogin("claude-a", &accounts[0], ReloginCause::RefreshGrant);

        let sibling = &pool.snapshot("claude-b", &accounts, None, None)[0];
        assert!(
            sibling.has_state && sibling.needs_relogin,
            "the sibling row carries the mark in the map but the dashboard still \
             reports it clean, so the fan-out is invisible where it matters"
        );
    }

    /// The mark's own mirror on the success path. A served response proves the
    /// credential is alive, which is as true for the sibling rows as for the one
    /// that carried the request; a clear narrower than the mark leaves a live
    /// credential condemned on every row that has not happened to serve yet.
    #[test]
    fn a_success_on_one_provider_row_clears_the_mark_on_its_siblings() {
        let pool = AccountPool::new();
        let shared = account("work");
        for provider in ["claude-a", "claude-b"] {
            pool.mark_healthy(provider, &shared, true);
        }
        pool.mark_needs_relogin("claude-a", &shared, ReloginCause::RefreshGrant);
        assert!(pool.needs_relogin("claude-b", &shared), "precondition");

        pool.mark_healthy("claude-b", &shared, true);

        assert!(
            !pool.needs_relogin("claude-a", &shared),
            "the sibling row stayed condemned after the credential served a \
             response through another provider row"
        );
    }

    /// The clear must reach exactly what the mark reaches. A relayed non-401 4xx
    /// proves the *credential* authenticated, which is as true for the sibling
    /// rows as for the one that served the request; a narrower clear would strand
    /// a mark the same evidence just disproved.
    #[test]
    fn clearing_the_mark_reaches_the_rows_the_marking_reached() {
        let pool = AccountPool::new();
        let shared = account("work");
        for provider in ["claude-a", "claude-b"] {
            pool.mark_healthy(provider, &shared, true);
        }
        pool.mark_needs_relogin("claude-a", &shared, ReloginCause::RefreshGrant);

        pool.clear_needs_relogin("claude-b", &shared);

        for provider in ["claude-a", "claude-b"] {
            assert!(
                !pool.needs_relogin(provider, &shared),
                "the {provider} row kept a mark the clear should have reached"
            );
        }
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
            .get(&account_key("anthropic", &accounts[0]))
            .is_some_and(|health| health.enabled));
    }

    #[test]
    fn syncing_enabled_accounts_replaces_upstream_membership() {
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

        let memberships = pool
            .memberships
            .lock()
            .expect("account membership lock poisoned");
        let current_membership = memberships.get("anthropic").unwrap();
        assert_eq!(current_membership.len(), 1);
        assert_eq!(
            current_membership.get(&account_key("anthropic", &current[0])),
            Some(&true)
        );
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

        let memberships = pool
            .memberships
            .lock()
            .expect("account membership lock poisoned");
        assert_eq!(
            memberships["anthropic"].get(&account_key("anthropic", &accounts[0])),
            Some(&true)
        );
    }

    #[test]
    fn pool_utilization_uses_best_enabled_non_stale_account() {
        let now = unix_now();
        let pool = AccountPool::new();
        let accounts = vec![account("best"), account("other")];
        pool.sync_enabled_accounts("anthropic", &accounts);
        pool.note_quota(
            "anthropic",
            &accounts[0],
            &quota_headers(&[
                ("anthropic-ratelimit-unified-5h-utilization", "0.2".into()),
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    (now + 3600).to_string(),
                ),
                ("anthropic-ratelimit-unified-7d-utilization", "0.6".into()),
                (
                    "anthropic-ratelimit-unified-7d-reset",
                    (now + 3600).to_string(),
                ),
            ]),
        );
        pool.note_quota(
            "anthropic",
            &accounts[1],
            &quota_headers(&[
                ("anthropic-ratelimit-unified-5h-utilization", "0.7".into()),
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    (now + 3600).to_string(),
                ),
                (
                    "anthropic-ratelimit-unified-7d_oi-utilization",
                    "0.4".into(),
                ),
                (
                    "anthropic-ratelimit-unified-7d_oi-reset",
                    (now + 3600).to_string(),
                ),
            ]),
        );
        let mut entries = pool.entries.lock().expect("account health lock poisoned");
        let (utilization, expired) = pool.pool_utilization_for("anthropic", &mut entries, now);
        assert_eq!(utilization, [Some(0.2), Some(0.6), Some(0.4)]);
        assert!(!expired, "the non-stale control must not report an expiry");
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
            rotation_reason(StatusCode::PAYMENT_REQUIRED, &HeaderMap::new()),
            "membership"
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
    fn verified_identity_shares_state_across_upstreams() {
        let pool = AccountPool::new();
        let first = account_with_uuid("explicit", "shared-uuid");
        let second = account_with_uuid("scanned", "shared-uuid");
        pool.cooldown("primary", &first, Duration::from_secs(60), "transport");

        let snapshot = pool.snapshot("secondary", &[second], None, None);
        assert!(snapshot[0].has_state);
        assert!(!snapshot[0].available);
        assert!(snapshot[0].cooldown_secs_remaining.is_some());
    }

    #[test]
    fn store_reference_and_store_scan_share_name_fallback() {
        let pool = AccountPool::new();
        let mut reference = account("acct-1");
        reference.store_entry = true;
        reference.store_family = Some(StoreFamily::Claude);
        let scanned = reference.clone();
        pool.note_quota(
            "explicit-upstream",
            &reference,
            &quota_headers(&[(
                "anthropic-ratelimit-unified-5h-utilization",
                "0.99".to_string(),
            )]),
        );

        let snapshot = pool.snapshot("whole-store", &[scanned], None, None);
        assert!(snapshot[0].near_quota);
        assert_eq!(snapshot[0].utilization_5h, Some(0.99));
    }

    #[test]
    fn uuidless_inline_name_fallback_is_upstream_scoped() {
        let pool = AccountPool::new();
        let first = account("same-name");
        let second = account("same-name");
        pool.cooldown("primary", &first, Duration::from_secs(60), "transport");

        let snapshot = pool.snapshot("secondary", &[second], None, None);
        assert!(!snapshot[0].has_state);
        assert!(snapshot[0].available);
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

        assert_eq!(collapse_representatives("anthropic", &accounts), vec![1, 3]);

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

        let before_second_call = unix_now();
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

        let entries = pool.entries.lock().unwrap();
        let observed_at_5h = entries
            .get(&account_key("codex", &accounts[0]))
            .unwrap()
            .quota
            .observed_at_5h;
        assert!(
            observed_at_5h.is_some_and(|at| at >= before_second_call),
            "the second call's utilization-only headers still stamp observed_at_5h"
        );
    }

    #[test]
    fn codex_missing_reset_stamps_observation_time() {
        // Both bucket arms stamp their observed_at unconditionally, even when
        // neither carries a reset header at all — not just on a later call
        // that already has a prior reset to preserve (that's
        // `codex_missing_reset_preserves_prior_reset`).
        let pool = AccountPool::new();
        let accounts = [account("pro")];
        let before = unix_now();
        pool.note_codex_quota(
            "codex",
            &accounts[0],
            &quota_headers(&[
                ("x-codex-primary-used-percent", "10".to_string()),
                ("x-codex-primary-window-minutes", "300".to_string()),
                ("x-codex-secondary-used-percent", "20".to_string()),
                ("x-codex-secondary-window-minutes", "10080".to_string()),
            ]),
        );

        let entries = pool.entries.lock().unwrap();
        let quota = &entries
            .get(&account_key("codex", &accounts[0]))
            .unwrap()
            .quota;
        assert!(quota.reset_5h.is_none());
        assert!(quota.reset_7d.is_none());
        assert!(
            quota.observed_at_5h.is_some_and(|at| at >= before),
            "the 5h bucket stamps observed_at even without a reset header"
        );
        assert!(
            quota.observed_at_7d.is_some_and(|at| at >= before),
            "the 7d bucket stamps observed_at even without a reset header"
        );
    }

    #[test]
    fn anthropic_aggregate_status_survives_stale_restored_reset() {
        let pool = AccountPool::new();
        let accounts = vec![account("a"), account("b")];
        let session = "anthropic-aggregate-after-restore";
        let initial = pool.select_order("anthropic", &accounts, Some(session), None, None);
        let sticky = initial[0];
        let future_reset = unix_now() + 3_600;
        pool.import_quotas([(
            account_key("anthropic", &accounts[sticky]),
            QuotaState {
                utilization_5h: Some(0.1),
                reset_5h: Some(future_reset),
                observed_at_5h: Some(unix_now()),
                ..Default::default()
            },
        )]);

        // Model the restored window reaching its reset after import. The
        // aggregate status below is then captured through the public header
        // path while the old per-window reset remains in the health entry.
        {
            let mut entries = pool.entries.lock().expect("account health lock poisoned");
            entries
                .get_mut(&account_key("anthropic", &accounts[sticky]))
                .expect("restored account exists")
                .quota
                .reset_5h = Some(unix_now().saturating_sub(1));
        }
        let before_status = unix_now();
        pool.note_quota(
            "anthropic",
            &accounts[sticky],
            &quota_headers(&[("anthropic-ratelimit-unified-status", "rejected".to_string())]),
        );

        let order = pool.select_order("anthropic", &accounts, Some(session), None, None);
        assert_ne!(
            order[0], sticky,
            "a fresh aggregate rejection remains selection-relevant after restore"
        );
        let entries = pool.entries.lock().unwrap();
        let quota = &entries
            .get(&account_key("anthropic", &accounts[sticky]))
            .unwrap()
            .quota;
        assert_eq!(quota.utilization_5h, None);
        assert_eq!(quota.reset_5h, None);
        assert_eq!(quota.status.as_deref(), Some("rejected"));
        assert!(quota
            .observed_at_status
            .is_some_and(|at| at >= before_status));
    }

    #[test]
    fn codex_aggregate_status_survives_stale_restored_reset() {
        let pool = AccountPool::new();
        let accounts = vec![account("a"), account("b")];
        let session = "codex-aggregate-after-restore";
        let initial = pool.select_order("codex", &accounts, Some(session), None, None);
        let sticky = initial[0];
        let future_reset = unix_now() + 3_600;
        pool.import_quotas([(
            account_key("codex", &accounts[sticky]),
            QuotaState {
                utilization_5h: Some(0.1),
                reset_5h: Some(future_reset),
                observed_at_5h: Some(unix_now()),
                ..Default::default()
            },
        )]);

        // Model the restored window reaching its reset after import, then
        // record Codex's aggregate reached-type status through its public path.
        {
            let mut entries = pool.entries.lock().expect("account health lock poisoned");
            entries
                .get_mut(&account_key("codex", &accounts[sticky]))
                .expect("restored account exists")
                .quota
                .reset_5h = Some(unix_now().saturating_sub(1));
        }
        let before_status = unix_now();
        pool.note_codex_quota(
            "codex",
            &accounts[sticky],
            &quota_headers(&[("x-codex-rate-limit-reached-type", "weekly".to_string())]),
        );

        let order = pool.select_order("codex", &accounts, Some(session), None, None);
        assert_eq!(
            order[0], sticky,
            "Codex's display-only weekly status does not rotate the sticky account"
        );
        let snapshots = pool.snapshot("codex", &accounts, None, None);
        let sticky_snapshot = snapshots
            .iter()
            .find(|snapshot| snapshot.name == accounts[sticky].name)
            .expect("sticky account snapshot exists");
        assert!(!sticky_snapshot.near_quota);
        let entries = pool.entries.lock().unwrap();
        let quota = &entries
            .get(&account_key("codex", &accounts[sticky]))
            .unwrap()
            .quota;
        assert_eq!(quota.utilization_5h, None);
        assert_eq!(quota.reset_5h, None);
        assert_eq!(quota.status.as_deref(), Some("weekly"));
        assert!(quota
            .observed_at_status
            .is_some_and(|at| at >= before_status));
    }

    #[test]
    fn codex_write_sweeps_a_passed_reset_before_replacing_utilization() {
        let pool = AccountPool::new();
        let account = account("codex-sweep");
        let now = unix_now();
        pool.note_codex_quota(
            "codex",
            &account,
            &quota_headers(&[
                ("x-codex-primary-used-percent", "40".to_string()),
                ("x-codex-primary-window-minutes", "300".to_string()),
                ("x-codex-primary-reset-at", (now + 3_600).to_string()),
                ("x-codex-rate-limit-reached-type", "rejected".to_string()),
            ]),
        );
        {
            let mut entries = pool.entries.lock().unwrap();
            let quota = &mut entries
                .get_mut(&account_key("codex", &account))
                .unwrap()
                .quota;
            // Model an old, unstamped aggregate from before independent
            // aggregate freshness existed, then pass the 5-hour reset before
            // the next Codex response replaces the utilization.
            quota.observed_at_status = None;
            quota.reset_5h = Some(now.saturating_sub(1));
        }

        pool.note_codex_quota(
            "codex",
            &account,
            &quota_headers(&[
                ("x-codex-primary-used-percent", "20".to_string()),
                ("x-codex-primary-window-minutes", "300".to_string()),
            ]),
        );

        let quota = pool
            .raw_quota_for_test(&account_key("codex", &account))
            .unwrap()
            .1;
        assert_eq!(quota.status, None);
        assert_eq!(quota.utilization_5h, Some(0.2));
        assert_eq!(quota.reset_5h, None);
    }

    #[test]
    fn generic_fresh_resetless_utilization_replaces_expired_reset() {
        let pool = AccountPool::new();
        let account = account("anthropic-account");
        let before = unix_now();
        pool.import_quotas([(
            account_key("anthropic", &account),
            QuotaState {
                utilization_5h: Some(0.91),
                reset_5h: Some(before.saturating_sub(1)),
                observed_at_5h: Some(before),
                ..Default::default()
            },
        )]);

        pool.note_quota(
            "anthropic",
            &account,
            &quota_headers(&[(
                "anthropic-ratelimit-unified-5h-utilization",
                "0.42".to_string(),
            )]),
        );
        let order = pool.select_order(
            "anthropic",
            std::slice::from_ref(&account),
            None,
            None,
            None,
        );
        assert_eq!(order, [0]);

        let entries = pool.entries.lock().unwrap();
        let quota = &entries
            .get(&account_key("anthropic", &account))
            .unwrap()
            .quota;
        assert_eq!(quota.utilization_5h, Some(0.42));
        assert_eq!(quota.reset_5h, None);
        assert!(quota.observed_at_5h.is_some_and(|at| at >= before));
    }

    #[test]
    fn codex_fresh_resetless_utilization_replaces_expired_reset() {
        let pool = AccountPool::new();
        let account = account("codex-account");
        let before = unix_now();
        pool.import_quotas([(
            account_key("codex", &account),
            QuotaState {
                utilization_5h: Some(0.91),
                reset_5h: Some(before.saturating_sub(1)),
                observed_at_5h: Some(before),
                ..Default::default()
            },
        )]);

        pool.note_codex_quota(
            "codex",
            &account,
            &quota_headers(&[
                ("x-codex-primary-used-percent", "42".to_string()),
                ("x-codex-primary-window-minutes", "300".to_string()),
            ]),
        );
        let order = pool.select_order("codex", std::slice::from_ref(&account), None, None, None);
        assert_eq!(order, [0]);

        let entries = pool.entries.lock().unwrap();
        let quota = &entries.get(&account_key("codex", &account)).unwrap().quota;
        assert_eq!(quota.utilization_5h, Some(0.42));
        assert_eq!(quota.reset_5h, None);
        assert!(quota.observed_at_5h.is_some_and(|at| at >= before));
    }

    #[test]
    fn generic_status_only_observation_replaces_expired_reset() {
        let pool = AccountPool::new();
        let account = account("status-account");
        let before = unix_now();
        pool.import_quotas([(
            account_key("anthropic", &account),
            QuotaState {
                reset_5h: Some(before.saturating_sub(1)),
                status_5h: Some("allowed".to_string()),
                observed_at_5h: Some(before),
                ..Default::default()
            },
        )]);

        pool.note_quota(
            "anthropic",
            &account,
            &quota_headers(&[(QUOTA_STATUS_HEADERS[0], "rejected".to_string())]),
        );
        let order = pool.select_order(
            "anthropic",
            std::slice::from_ref(&account),
            None,
            None,
            None,
        );
        assert_eq!(order, [0]);

        let entries = pool.entries.lock().unwrap();
        let quota = &entries
            .get(&account_key("anthropic", &account))
            .unwrap()
            .quota;
        assert_eq!(quota.status_5h.as_deref(), Some("rejected"));
        assert_eq!(quota.reset_5h, None);
        assert!(quota.observed_at_status_5h.is_some_and(|at| at >= before));
    }

    #[test]
    fn generic_fresh_resetless_utilization_preserves_future_reset() {
        let pool = AccountPool::new();
        let account = account("future-reset-account");
        let before = unix_now();
        let reset = before + 3_600;
        pool.import_quotas([(
            account_key("anthropic", &account),
            QuotaState {
                utilization_5h: Some(0.2),
                reset_5h: Some(reset),
                observed_at_5h: Some(before),
                ..Default::default()
            },
        )]);

        pool.note_quota(
            "anthropic",
            &account,
            &quota_headers(&[(
                "anthropic-ratelimit-unified-5h-utilization",
                "0.42".to_string(),
            )]),
        );
        pool.select_order(
            "anthropic",
            std::slice::from_ref(&account),
            None,
            None,
            None,
        );

        let entries = pool.entries.lock().unwrap();
        let quota = &entries
            .get(&account_key("anthropic", &account))
            .unwrap()
            .quota;
        assert_eq!(quota.utilization_5h, Some(0.42));
        assert_eq!(quota.reset_5h, Some(reset));
    }

    #[test]
    fn generic_reset_only_header_updates_metadata_without_observation() {
        let pool = AccountPool::new();
        let account = account("metadata-account");
        let reset = unix_now() + 3_600;
        pool.note_quota(
            "anthropic",
            &account,
            &quota_headers(&[("anthropic-ratelimit-unified-5h-reset", reset.to_string())]),
        );
        pool.select_order(
            "anthropic",
            std::slice::from_ref(&account),
            None,
            None,
            None,
        );

        let entries = pool.entries.lock().unwrap();
        let quota = &entries
            .get(&account_key("anthropic", &account))
            .unwrap()
            .quota;
        assert_eq!(quota.reset_5h, Some(reset));
        assert_eq!(quota.utilization_5h, None);
        assert_eq!(quota.observed_at_5h, None);
    }

    #[test]
    fn codex_reset_only_header_updates_metadata_without_observation() {
        let pool = AccountPool::new();
        let account = account("codex-metadata-account");
        let reset = unix_now() + 3_600;
        pool.note_codex_quota(
            "codex",
            &account,
            &quota_headers(&[
                ("x-codex-primary-window-minutes", "300".to_string()),
                ("x-codex-primary-reset-at", reset.to_string()),
            ]),
        );
        pool.select_order("codex", std::slice::from_ref(&account), None, None, None);

        let entries = pool.entries.lock().unwrap();
        let quota = &entries.get(&account_key("codex", &account)).unwrap().quota;
        assert_eq!(quota.reset_5h, Some(reset));
        assert_eq!(quota.utilization_5h, None);
        assert_eq!(quota.observed_at_5h, None);
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
    fn account_reenters_selection_after_reset_passes() {
        // Regression: a sticky account exhausted with a future reset must
        // rejoin the head of selection once that reset has actually passed —
        // the pre-fix bug left reset-carrying marks stuck too, not just
        // reset-less ones, whenever the mark outlived its own reset.
        let pool = AccountPool::new();
        let accounts = vec![account("a"), account("b")];
        let session = "reenter-reset-passes";
        let initial = pool.select_order("codex", &accounts, Some(session), None, None);
        let sticky = initial[0];
        let reset = unix_now() + 3_600;
        pool.note_codex_quota(
            "codex",
            &accounts[sticky],
            &quota_headers(&[
                ("x-codex-primary-used-percent", "100".to_string()),
                ("x-codex-primary-window-minutes", "300".to_string()),
                ("x-codex-primary-reset-at", reset.to_string()),
            ]),
        );
        let yielded = pool.select_order("codex", &accounts, Some(session), None, None);
        assert_ne!(
            yielded[0], sticky,
            "an exhausted account yields while its reset is still future"
        );

        // Rewind the reset into the past directly — this is the state the
        // account would be in once upstream's window has actually reset, with
        // no need to sleep in the test.
        {
            let mut entries = pool.entries.lock().expect("account health lock poisoned");
            let health = entries
                .get_mut(&account_key("codex", &accounts[sticky]))
                .unwrap();
            health.quota.reset_5h = Some(unix_now() - 1);
        }

        let recovered = pool.select_order("codex", &accounts, Some(session), None, None);
        assert_eq!(
            recovered[0], sticky,
            "the account re-enters selection once its reset has passed"
        );
        let snaps = pool.snapshot("codex", &accounts, None, None);
        let sticky_snap = snaps
            .iter()
            .find(|snap| snap.name == accounts[sticky].name)
            .unwrap();
        assert!(
            sticky_snap.available,
            "the recovered account is available again"
        );
    }

    #[test]
    fn account_reenters_selection_after_reset_less_mark_ages_out() {
        // Reproduces the incident this change fixes: a deployed multi-account
        // codex pool recorded a valid window-minutes group with utilization
        // above threshold but a blank reset-at header, so no reset instant was
        // ever captured and the near-quota mark never expired on its own.
        let pool_cfg = PoolConfig {
            default_threshold: Some(0.80),
            ..Default::default()
        };
        let pool = AccountPool::new();
        let accounts = vec![account("a"), account("b")];
        let session = "reenter-reset-less";
        let initial = pool.select_order("codex", &accounts, Some(session), None, Some(&pool_cfg));
        let sticky = initial[0];
        pool.note_codex_quota(
            "codex",
            &accounts[sticky],
            &quota_headers(&[
                ("x-codex-primary-used-percent", "84".to_string()),
                ("x-codex-primary-window-minutes", "300".to_string()),
                ("x-codex-primary-reset-at", String::new()),
            ]),
        );
        let yielded = pool.select_order("codex", &accounts, Some(session), None, Some(&pool_cfg));
        assert_ne!(
            yielded[0], sticky,
            "the near-quota account yields immediately"
        );

        // Rewind the observation past the 5h window length — no restart and no
        // real time passage needed, just the state one window length later
        // would look like.
        {
            let mut entries = pool.entries.lock().expect("account health lock poisoned");
            let health = entries
                .get_mut(&account_key("codex", &accounts[sticky]))
                .unwrap();
            health.quota.observed_at_5h = Some(unix_now() - WINDOW_5H_SECS);
        }

        let recovered = pool.select_order("codex", &accounts, Some(session), None, Some(&pool_cfg));
        assert_eq!(
            recovered[0], sticky,
            "the reset-less mark ages out and the account re-enters selection"
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
                .get_mut(&account_key("codex", &acc))
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
    fn forget_identity_is_store_family_scoped() {
        let pool = AccountPool::new();
        let mut anthropic = account("main");
        anthropic.store_family = Some(StoreFamily::Claude);
        anthropic.store_entry = true;
        let mut codex = account("main");
        codex.store_family = Some(StoreFamily::Chatgpt);
        codex.store_entry = true;
        pool.cooldown(
            "anthropic",
            &anthropic,
            Duration::from_secs(60),
            "transport",
        );
        pool.cooldown("codex", &codex, Duration::from_secs(60), "transport");
        let old_codex_lock = pool.refresh_lock("codex", &codex);
        let anthropic_lock = pool.refresh_lock("anthropic", &anthropic);

        pool.forget_identity(StoreFamily::Chatgpt, "main");

        let new_codex_lock = pool.refresh_lock("codex", &codex);
        assert!(!Arc::ptr_eq(&old_codex_lock, &new_codex_lock));
        assert!(Arc::ptr_eq(
            &anthropic_lock,
            &pool.refresh_lock("anthropic", &anthropic)
        ));

        assert!(!pool.snapshot("codex", &[codex], None, None)[0].has_state);
        assert!(pool.snapshot("anthropic", &[anthropic], None, None)[0].has_state);
    }

    #[test]
    fn unstamped_account_key_infers_kimi_rather_than_defaulting_to_claude() {
        // `account_key` guesses a store family only when the caller bypassed
        // `resolve_pool_accounts` (which always stamps one). The guess used to
        // be "codex/chatgpt, else Claude", which silently filed a Kimi account
        // under the Claude family — note that `kimi-code`, the preset's own
        // upstream name, does not contain `codex`, so it took the Claude
        // branch, and two same-named store accounts collided on one key.
        //
        // Both accounts are `store_entry`, so their identity is the bare
        // account name with no upstream in it. That makes `store_family` the
        // only component that can distinguish the two keys — an inline account
        // would differ by upstream name alone and prove nothing about family.
        let store_account = |name: &str| AccountConfig {
            store_entry: true,
            ..account(name)
        };
        let kimi = store_account("shared-name");
        let claude = store_account("shared-name");
        assert_eq!(kimi.store_family, None, "the guess only applies unstamped");

        assert_ne!(
            account_key("kimi-code", &kimi),
            account_key("anthropic", &claude),
            "a Kimi account must not share a pool-state key with a Claude account of the same name"
        );

        // The stamped family still wins over the upstream-name guess.
        let stamped = AccountConfig {
            store_family: Some(StoreFamily::Claude),
            ..store_account("shared-name")
        };
        assert_eq!(
            account_key("kimi-code", &stamped),
            account_key("anthropic", &claude),
            "an explicitly stamped family must override the upstream-name guess"
        );
    }

    #[test]
    fn forget_identity_purges_membership_entries() {
        // `forget_identity` must clear the forgotten key from the membership map
        // too, not just `entries`/`refresh_locks` — otherwise a deleted/rotated
        // account leaves dead `AccountKey`s accumulating there.
        let pool = AccountPool::new();
        let mut codex = account("main");
        codex.store_family = Some(StoreFamily::Chatgpt);
        codex.store_entry = true;
        pool.sync_enabled_accounts("codex", std::slice::from_ref(&codex));
        let key = account_key("codex", &codex);
        assert!(
            pool.memberships
                .lock()
                .unwrap()
                .get("codex")
                .is_some_and(|members| members.contains_key(&key)),
            "sync should have recorded the account in the membership map"
        );

        pool.forget_identity(StoreFamily::Chatgpt, "main");

        assert!(
            !pool
                .memberships
                .lock()
                .unwrap()
                .get("codex")
                .is_some_and(|members| members.contains_key(&key)),
            "forget_identity should purge the forgotten key from the membership map"
        );
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
    fn fable_scoped_cooldown_only_defers_fable_requests() {
        let pool = AccountPool::new();
        let accounts = vec![account("a"), account("b")];
        let session = "fable-cooldown";
        let rotation = pool.select_order(
            "anthropic",
            &accounts,
            Some(session),
            Some("claude-sonnet-5"),
            None,
        );
        let sticky = rotation[0];
        let headers = quota_headers(&[(QUOTA_STATUS_HEADERS[2], "rejected".to_string())]);
        assert!(is_fable_scoped_rejection(&headers));
        pool.cooldown_scoped(
            "anthropic",
            &accounts[sticky],
            Duration::from_secs(60),
            "quota",
            CooldownScope::Fable,
        );

        let sonnet_order = pool.select_order(
            "anthropic",
            &accounts,
            Some(session),
            Some("claude-sonnet-5"),
            None,
        );
        assert_eq!(
            sonnet_order[0], sticky,
            "a Fable-only cooldown must leave the sticky account available to Sonnet"
        );
        let sonnet_snapshot = &pool.snapshot(
            "anthropic",
            std::slice::from_ref(&accounts[sticky]),
            Some("claude-sonnet-5"),
            None,
        )[0];
        assert!(sonnet_snapshot.available);
        assert!(sonnet_snapshot.cooldown_fable_secs_remaining.is_some());
        assert_eq!(
            pool.select_order(
                "anthropic",
                &accounts,
                Some(session),
                Some("claude-fable-5"),
                None,
            )
            .last(),
            Some(&sticky),
            "the Fable request must put the cooled account in the tail"
        );
    }

    #[test]
    fn shared_rejection_cooldown_defers_both_model_families() {
        let pool = AccountPool::new();
        let accounts = vec![account("a"), account("b")];
        let session = "shared-cooldown";
        let rotation = pool.select_order(
            "anthropic",
            &accounts,
            Some(session),
            Some("claude-sonnet-5"),
            None,
        );
        let sticky = rotation[0];
        let headers = quota_headers(&[(QUOTA_STATUS_HEADERS[0], "rejected".to_string())]);
        assert!(!is_fable_scoped_rejection(&headers));
        pool.cooldown_scoped(
            "anthropic",
            &accounts[sticky],
            Duration::from_secs(60),
            "quota",
            CooldownScope::Account,
        );

        for model in ["claude-sonnet-5", "claude-fable-5"] {
            assert_eq!(
                pool.select_order("anthropic", &accounts, Some(session), Some(model), None,)
                    .last(),
                Some(&sticky),
                "account-wide cooldown must defer {model}"
            );
        }
    }

    #[test]
    fn healthy_mark_clears_cooldowns_at_the_request_scope() {
        let pool = AccountPool::new();
        let account = account("a");
        pool.cooldown_scoped(
            "anthropic",
            &account,
            Duration::from_secs(60),
            "quota",
            CooldownScope::Fable,
        );
        pool.mark_healthy("anthropic", &account, true);
        assert!(
            pool.snapshot("anthropic", std::slice::from_ref(&account), None, None)[0]
                .cooldown_fable_secs_remaining
                .is_some(),
            "a non-Fable success proves nothing about the Fable bucket"
        );

        pool.cooldown("anthropic", &account, Duration::from_secs(60), "transport");
        pool.mark_healthy_scoped("anthropic", &account, true, true);
        let snapshot = &pool.snapshot(
            "anthropic",
            std::slice::from_ref(&account),
            Some("claude-fable-5"),
            None,
        )[0];
        assert!(snapshot.cooldown_secs_remaining.is_none());
        assert!(snapshot.cooldown_fable_secs_remaining.is_none());
    }

    #[test]
    fn per_window_rejections_govern_only_their_models() {
        let account = account("a");
        let now = unix_now();
        let fable_rejected = QuotaState {
            utilization_7d_oi: Some(0.1),
            status_7d_oi: Some("rejected".to_string()),
            ..Default::default()
        };
        let fable = assess_quota(&fable_rejected, &account, true, None, now);
        let sonnet = assess_quota(&fable_rejected, &account, false, None, now);
        assert!(fable.near);
        assert_eq!(fable.headroom, f64::NEG_INFINITY);
        assert!(!sonnet.near);
        assert_eq!(sonnet.headroom, f64::INFINITY);

        let five_hour_rejected = QuotaState {
            status_5h: Some("rejected".to_string()),
            ..Default::default()
        };
        for is_fable in [false, true] {
            let assessment = assess_quota(&five_hour_rejected, &account, is_fable, None, now);
            assert!(assessment.near);
            assert_eq!(assessment.headroom, f64::NEG_INFINITY);
        }
    }

    #[test]
    fn fable_rejection_status_does_not_require_utilization() {
        let account = account("a");
        let quota = QuotaState {
            status_7d_oi: Some("rejected".to_string()),
            ..Default::default()
        };

        let fable = assess_quota(&quota, &account, true, None, unix_now());
        assert!(fable.near);
        assert_eq!(fable.headroom, f64::NEG_INFINITY);

        let non_fable = assess_quota(&quota, &account, false, None, unix_now());
        assert!(!non_fable.near);
        assert_eq!(non_fable.headroom, f64::INFINITY);
    }

    #[test]
    fn shared_weekly_rejection_governs_fable_without_oi_data() {
        let quota = QuotaState {
            status_7d: Some("rejected".to_string()),
            ..Default::default()
        };

        // The shared weekly rejection is unconditional: it governs Fable
        // requests (which have no `7d_oi` data to fall back on here) and
        // ordinary requests alike. Folding this term inside the `is_fable`
        // gate would silently stop non-Fable traffic from ever being marked
        // quota-rejected on a shared weekly rejection.
        for is_fable in [false, true] {
            let assessment = assess_quota(&quota, &account("a"), is_fable, None, unix_now());
            assert!(
                assessment.near,
                "shared 7d rejection governs is_fable={is_fable}"
            );
            assert_eq!(assessment.headroom, f64::NEG_INFINITY);
        }
    }

    #[test]
    fn per_window_status_suppresses_legacy_aggregate_rejection() {
        let quota = QuotaState {
            status: Some("rejected".to_string()),
            status_7d: Some("allowed".to_string()),
            ..Default::default()
        };

        for is_fable in [false, true] {
            let assessment = assess_quota(&quota, &account("a"), is_fable, None, unix_now());
            assert!(!assessment.near);
            assert_eq!(assessment.headroom, f64::INFINITY);
        }
    }

    #[test]
    fn aggregate_rejection_is_a_legacy_fallback_without_window_statuses() {
        let quota = QuotaState {
            status: Some("rejected".to_string()),
            ..Default::default()
        };
        for is_fable in [false, true] {
            let assessment = assess_quota(&quota, &account("a"), is_fable, None, unix_now());
            assert!(assessment.near);
            assert_eq!(assessment.headroom, f64::NEG_INFINITY);
        }
    }

    #[test]
    fn stale_window_clears_only_its_status_and_the_legacy_aggregate() {
        let now = unix_now();
        let mut quota = QuotaState {
            utilization_5h: Some(1.0),
            reset_5h: Some(now),
            utilization_7d: Some(0.4),
            reset_7d: Some(now + 60),
            utilization_7d_oi: Some(0.5),
            reset_7d_oi: Some(now + 120),
            status: Some("rejected".to_string()),
            status_5h: Some("rejected".to_string()),
            status_7d: Some("allowed".to_string()),
            status_7d_oi: Some("rejected".to_string()),
            observed_at_5h: None,
            observed_at_7d: None,
            observed_at_7d_oi: None,
            observed_at_status_5h: Some(now),
            observed_at_status_7d: Some(now),
            observed_at_status_7d_oi: Some(now),
            reset_at_status_5h: Some(now),
            reset_at_status_7d: Some(now + 60),
            reset_at_status_7d_oi: Some(now + 120),
            observed_at_status: None,
        };
        expire_stale_quota(&mut quota, now);
        assert_eq!(quota.status_5h, None);
        assert_eq!(quota.status_7d.as_deref(), Some("allowed"));
        assert_eq!(quota.status_7d_oi.as_deref(), Some("rejected"));
        assert_eq!(quota.status, None);

        expire_stale_quota(&mut quota, now + 60);
        assert_eq!(quota.status_7d, None);
        assert_eq!(quota.status_7d_oi.as_deref(), Some("rejected"));
        expire_stale_quota(&mut quota, now + 120);
        assert_eq!(quota.status_7d_oi, None);
    }

    #[test]
    fn reset_less_window_expires_one_window_length_after_observation() {
        let now = unix_now();
        let mut quota = QuotaState {
            utilization_7d: Some(0.9),
            observed_at_7d: Some(now - WINDOW_7D_SECS + 1),
            status: Some("rejected".to_string()),
            observed_at_status: Some(now),
            ..Default::default()
        };
        // One second short of the boundary: the reset-less mark is still alive.
        expire_stale_quota(&mut quota, now);
        assert_eq!(quota.utilization_7d, Some(0.9));
        assert_eq!(quota.observed_at_7d, Some(now - WINDOW_7D_SECS + 1));

        // At the boundary (observed_at + window_len == now), only the stale
        // window is cleared. A stamped aggregate has its own independent cap.
        quota.observed_at_7d = Some(now - WINDOW_7D_SECS);
        expire_stale_quota(&mut quota, now);
        assert_eq!(quota.utilization_7d, None);
        assert_eq!(quota.observed_at_7d, None);
        assert_eq!(quota.status.as_deref(), Some("rejected"));
        assert_eq!(quota.observed_at_status, Some(now));
    }

    #[test]
    fn stale_window_preserves_fresh_stamped_aggregate() {
        let now = unix_now();
        let cases = [
            (
                "5h",
                QuotaState {
                    utilization_5h: Some(0.9),
                    reset_5h: Some(now),
                    ..Default::default()
                },
                false,
            ),
            (
                "7d",
                QuotaState {
                    utilization_7d: Some(0.9),
                    reset_7d: Some(now),
                    ..Default::default()
                },
                false,
            ),
            (
                "7d_oi",
                QuotaState {
                    utilization_7d_oi: Some(0.9),
                    reset_7d_oi: Some(now),
                    ..Default::default()
                },
                true,
            ),
        ];

        for (window, mut quota, is_fable) in cases {
            quota.status = Some("rejected".to_string());
            quota.observed_at_status = Some(now);
            expire_stale_quota(&mut quota, now);

            assert!(
                quota.utilization_5h.is_none()
                    && quota.utilization_7d.is_none()
                    && quota.utilization_7d_oi.is_none(),
                "stale {window} utilization must be cleared"
            );
            assert!(
                quota.reset_5h.is_none() && quota.reset_7d.is_none() && quota.reset_7d_oi.is_none(),
                "stale {window} reset must be cleared"
            );
            assert_eq!(
                quota.status.as_deref(),
                Some("rejected"),
                "fresh aggregate status must survive stale {window}"
            );
            assert_eq!(
                quota.observed_at_status,
                Some(now),
                "fresh aggregate timestamp must survive stale {window}"
            );
            assert!(
                assess_quota(&quota, &account("a"), is_fable, None, now).near,
                "aggregate rejection must remain near after stale {window} cleanup"
            );
        }
    }

    #[test]
    fn aggregate_only_status_expires_after_longest_window() {
        let now = unix_now();
        let mut quota = QuotaState {
            status: Some("rejected".to_string()),
            observed_at_status: Some(now - WINDOW_7D_SECS + 1),
            ..Default::default()
        };
        expire_stale_quota(&mut quota, now);
        assert_eq!(
            quota.status.as_deref(),
            Some("rejected"),
            "not yet at the boundary"
        );

        quota.observed_at_status = Some(now - WINDOW_7D_SECS);
        expire_stale_quota(&mut quota, now);
        assert_eq!(quota.status, None);
        assert_eq!(quota.observed_at_status, None);
    }

    #[test]
    fn aggregate_cap_clears_status_even_with_live_window_signal() {
        // Revision 3 (P2-1): a usage poller (or any other path) can keep a
        // window's utilization/reset fresh indefinitely without ever touching
        // `status`, so a stale aggregate-only rejection needs an unconditional
        // lifetime cap independent of whether a window signal is still alive —
        // otherwise it would never expire on its own.
        let now = unix_now();
        let mut quota = QuotaState {
            utilization_7d: Some(0.5),
            reset_7d: Some(now + WINDOW_7D_SECS),
            status: Some("rejected".to_string()),
            observed_at_status: Some(now - WINDOW_7D_SECS),
            ..Default::default()
        };
        expire_stale_quota(&mut quota, now);
        assert_eq!(
            quota.status, None,
            "the aggregate cap fires regardless of window health"
        );
        assert_eq!(quota.observed_at_status, None);
        assert_eq!(
            quota.utilization_7d,
            Some(0.5),
            "the live window signal is untouched"
        );
        assert_eq!(quota.reset_7d, Some(now + WINDOW_7D_SECS));
    }

    #[test]
    fn per_window_status_expires_independently_from_utilization() {
        let now = unix_now();
        let mut quota = QuotaState {
            utilization_5h: Some(0.1),
            reset_5h: Some(now + 3_600),
            observed_at_5h: Some(now),
            status_5h: Some("rejected".to_string()),
            observed_at_status_5h: Some(now - WINDOW_5H_SECS),
            ..Default::default()
        };

        expire_stale_quota(&mut quota, now);

        assert_eq!(quota.utilization_5h, Some(0.1));
        assert_eq!(quota.reset_5h, Some(now + 3_600));
        assert_eq!(quota.status_5h, None);
        assert_eq!(quota.observed_at_status_5h, None);
    }

    #[test]
    fn captured_reset_and_timestamp_cap_use_the_earlier_status_boundary() {
        let now = unix_now();
        let mut cases = [
            QuotaState {
                status_5h: Some("rejected".to_string()),
                observed_at_status_5h: Some(now - WINDOW_5H_SECS + 1),
                reset_at_status_5h: Some(now - 1),
                ..Default::default()
            },
            QuotaState {
                status_5h: Some("rejected".to_string()),
                observed_at_status_5h: Some(now - WINDOW_5H_SECS),
                reset_at_status_5h: Some(now + 3_600),
                ..Default::default()
            },
            QuotaState {
                status_7d: Some("rejected".to_string()),
                observed_at_status_7d: Some(now - WINDOW_7D_SECS),
                reset_at_status_7d: Some(now + 3_600),
                ..Default::default()
            },
            QuotaState {
                status_7d: Some("rejected".to_string()),
                observed_at_status_7d: Some(now - WINDOW_7D_SECS + 1),
                reset_at_status_7d: Some(now - 1),
                ..Default::default()
            },
            QuotaState {
                status_7d_oi: Some("rejected".to_string()),
                observed_at_status_7d_oi: Some(now - WINDOW_7D_SECS),
                reset_at_status_7d_oi: Some(now + 3_600),
                ..Default::default()
            },
            QuotaState {
                status_7d_oi: Some("rejected".to_string()),
                observed_at_status_7d_oi: Some(now - WINDOW_7D_SECS + 1),
                reset_at_status_7d_oi: Some(now - 1),
                ..Default::default()
            },
        ];

        for quota in &mut cases {
            expire_stale_quota(quota, now);
            assert!(quota.status_5h.is_none());
            assert!(quota.status_7d.is_none());
            assert!(quota.status_7d_oi.is_none());
            assert!(quota.observed_at_status_5h.is_none());
            assert!(quota.observed_at_status_7d.is_none());
            assert!(quota.observed_at_status_7d_oi.is_none());
            assert!(quota.reset_at_status_5h.is_none());
            assert!(quota.reset_at_status_7d.is_none());
            assert!(quota.reset_at_status_7d_oi.is_none());
        }
    }

    #[test]
    fn utilization_cap_preserves_a_future_reset_for_each_window() {
        let now = unix_now();
        let future = now + 3_600;
        let mut cases = [
            QuotaState {
                utilization_5h: Some(0.1),
                reset_5h: Some(future),
                observed_at_5h: Some(now - WINDOW_5H_SECS),
                ..Default::default()
            },
            QuotaState {
                utilization_7d: Some(0.2),
                reset_7d: Some(future),
                observed_at_7d: Some(now - WINDOW_7D_SECS),
                ..Default::default()
            },
            QuotaState {
                utilization_7d_oi: Some(0.3),
                reset_7d_oi: Some(future),
                observed_at_7d_oi: Some(now - WINDOW_7D_SECS),
                ..Default::default()
            },
        ];

        for quota in &mut cases {
            expire_stale_quota(quota, now);
        }
        assert_eq!(cases[0].utilization_5h, None);
        assert_eq!(cases[0].observed_at_5h, None);
        assert_eq!(cases[0].reset_5h, Some(future));
        assert_eq!(cases[1].utilization_7d, None);
        assert_eq!(cases[1].observed_at_7d, None);
        assert_eq!(cases[1].reset_7d, Some(future));
        assert_eq!(cases[2].utilization_7d_oi, None);
        assert_eq!(cases[2].observed_at_7d_oi, None);
        assert_eq!(cases[2].reset_7d_oi, Some(future));
    }

    #[test]
    fn stale_status_preserves_fresh_utilization_and_the_reverse() {
        let now = unix_now();
        let mut status_stale = QuotaState {
            utilization_7d: Some(0.2),
            reset_7d: Some(now + 3_600),
            observed_at_7d: Some(now),
            status_7d: Some("rejected".to_string()),
            observed_at_status_7d: Some(now - WINDOW_7D_SECS),
            ..Default::default()
        };
        expire_stale_quota(&mut status_stale, now);
        assert_eq!(status_stale.utilization_7d, Some(0.2));
        assert_eq!(status_stale.status_7d, None);

        let mut utilization_stale = QuotaState {
            utilization_7d_oi: Some(0.2),
            reset_7d_oi: Some(now + 3_600),
            observed_at_7d_oi: Some(now - WINDOW_7D_SECS),
            status_7d_oi: Some("allowed".to_string()),
            observed_at_status_7d_oi: Some(now),
            reset_at_status_7d_oi: Some(now + 3_600),
            ..Default::default()
        };
        expire_stale_quota(&mut utilization_stale, now);
        assert_eq!(utilization_stale.utilization_7d_oi, None);
        assert_eq!(utilization_stale.reset_7d_oi, Some(now + 3_600));
        assert_eq!(utilization_stale.status_7d_oi.as_deref(), Some("allowed"));
        assert_eq!(utilization_stale.reset_at_status_7d_oi, Some(now + 3_600));
    }

    #[test]
    fn stale_status_and_utilization_are_independent_for_each_window() {
        let now = unix_now();
        let future = now + 3_600;
        let mut cases = [
            (
                QuotaState {
                    utilization_5h: Some(0.1),
                    reset_5h: Some(future),
                    observed_at_5h: Some(now),
                    status_5h: Some("rejected".to_string()),
                    observed_at_status_5h: Some(now - WINDOW_5H_SECS),
                    reset_at_status_5h: Some(future),
                    ..Default::default()
                },
                true,
                false,
            ),
            (
                QuotaState {
                    utilization_5h: Some(0.1),
                    reset_5h: Some(future),
                    observed_at_5h: Some(now - WINDOW_5H_SECS),
                    status_5h: Some("allowed".to_string()),
                    observed_at_status_5h: Some(now),
                    reset_at_status_5h: Some(future),
                    ..Default::default()
                },
                false,
                true,
            ),
            (
                QuotaState {
                    utilization_7d: Some(0.2),
                    reset_7d: Some(future),
                    observed_at_7d: Some(now),
                    status_7d: Some("rejected".to_string()),
                    observed_at_status_7d: Some(now - WINDOW_7D_SECS),
                    reset_at_status_7d: Some(future),
                    ..Default::default()
                },
                true,
                false,
            ),
            (
                QuotaState {
                    utilization_7d: Some(0.2),
                    reset_7d: Some(future),
                    observed_at_7d: Some(now - WINDOW_7D_SECS),
                    status_7d: Some("allowed".to_string()),
                    observed_at_status_7d: Some(now),
                    reset_at_status_7d: Some(future),
                    ..Default::default()
                },
                false,
                true,
            ),
            (
                QuotaState {
                    utilization_7d_oi: Some(0.3),
                    reset_7d_oi: Some(future),
                    observed_at_7d_oi: Some(now),
                    status_7d_oi: Some("rejected".to_string()),
                    observed_at_status_7d_oi: Some(now - WINDOW_7D_SECS),
                    reset_at_status_7d_oi: Some(future),
                    ..Default::default()
                },
                true,
                false,
            ),
            (
                QuotaState {
                    utilization_7d_oi: Some(0.3),
                    reset_7d_oi: Some(future),
                    observed_at_7d_oi: Some(now - WINDOW_7D_SECS),
                    status_7d_oi: Some("allowed".to_string()),
                    observed_at_status_7d_oi: Some(now),
                    reset_at_status_7d_oi: Some(future),
                    ..Default::default()
                },
                false,
                true,
            ),
        ];

        for (quota, utilization_live, status_live) in &mut cases {
            expire_stale_quota(quota, now);
            assert_eq!(
                quota.utilization_5h.is_some()
                    || quota.utilization_7d.is_some()
                    || quota.utilization_7d_oi.is_some(),
                *utilization_live
            );
            assert_eq!(
                quota.status_5h.is_some()
                    || quota.status_7d.is_some()
                    || quota.status_7d_oi.is_some(),
                *status_live
            );
        }
    }

    #[test]
    fn stale_rejection_stops_affecting_selection_below_threshold() {
        let pool = AccountPool::new();
        let accounts = [account("a"), account("b")];
        let sticky = pool.select_order("anthropic", &accounts, Some("stale"), None, None)[0];
        pool.note_quota(
            "anthropic",
            &accounts[sticky],
            &quota_headers(&[
                (QUOTA_STATUS_HEADERS[0], "rejected".to_string()),
                (
                    "anthropic-ratelimit-unified-5h-utilization",
                    "0.1".to_string(),
                ),
            ]),
        );
        {
            let mut entries = pool.entries.lock().unwrap();
            let quota = &mut entries
                .get_mut(&account_key("anthropic", &accounts[sticky]))
                .unwrap()
                .quota;
            quota.observed_at_status_5h = Some(unix_now().saturating_sub(WINDOW_5H_SECS));
        }

        let snapshots = pool.snapshot("anthropic", &accounts, None, None);
        let sticky_snapshot = snapshots
            .iter()
            .find(|snapshot| snapshot.name == accounts[sticky].name)
            .unwrap();
        assert!(!sticky_snapshot.near_quota);
        assert_eq!(sticky_snapshot.utilization_5h, Some(0.1));
        assert_eq!(sticky_snapshot.status, None);
    }

    #[test]
    fn usage_replaces_a_passed_reset_only_after_sweeping_old_status() {
        let pool = AccountPool::new();
        let account = account("usage-sweep");
        let now = unix_now();
        pool.note_quota(
            "anthropic",
            &account,
            &quota_headers(&[
                (QUOTA_STATUS_HEADERS[0], "rejected".to_string()),
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    (now + 3_600).to_string(),
                ),
            ]),
        );
        {
            let mut entries = pool.entries.lock().unwrap();
            let quota = &mut entries
                .get_mut(&account_key("anthropic", &account))
                .unwrap()
                .quota;
            quota.reset_5h = Some(now.saturating_sub(1));
            quota.observed_at_status_5h = Some(now.saturating_sub(1));
            quota.reset_at_status_5h = Some(now.saturating_sub(1));
        }

        pool.note_usage(
            "anthropic",
            &account,
            &UsageSnapshot {
                five_hour: Some(UsageWindow {
                    utilization: 0.1,
                    resets_at: Some(now + 3_600),
                }),
                ..Default::default()
            },
        );

        let entries = pool.entries.lock().unwrap();
        let quota = &entries
            .get(&account_key("anthropic", &account))
            .unwrap()
            .quota;
        assert_eq!(quota.status_5h, None);
        assert_eq!(quota.reset_at_status_5h, None);
        assert_eq!(quota.utilization_5h, Some(0.1));
        assert_eq!(quota.reset_5h, Some(now + 3_600));
    }

    #[test]
    fn old_quota_json_deserializes_without_per_window_statuses() {
        let quota: QuotaState = serde_json::from_str(
            r#"{"utilization_5h":0.5,"reset_5h":1800000000,"status":"rejected"}"#,
        )
        .unwrap();
        assert_eq!(quota.status.as_deref(), Some("rejected"));
        assert_eq!(quota.status_5h, None);
        assert_eq!(quota.status_7d, None);
        assert_eq!(quota.status_7d_oi, None);
    }

    #[test]
    fn legacy_import_migrates_each_window_signal_shape() {
        let now = unix_now();
        let reset = now + 3_600;
        let cases = [
            QuotaState {
                utilization_5h: Some(0.1),
                observed_at_5h: Some(now - 60),
                ..Default::default()
            },
            QuotaState {
                status_7d: Some("rejected".to_string()),
                reset_7d: Some(reset),
                observed_at_7d: Some(now - 60),
                ..Default::default()
            },
            QuotaState {
                utilization_7d_oi: Some(0.2),
                status_7d_oi: Some("allowed".to_string()),
                reset_7d_oi: Some(reset),
                observed_at_7d_oi: Some(now - 60),
                ..Default::default()
            },
            QuotaState {
                reset_5h: Some(reset),
                ..Default::default()
            },
            QuotaState {
                observed_at_5h: Some(now - 60),
                ..Default::default()
            },
        ];
        let pool = AccountPool::new();
        let accounts = (0..cases.len())
            .map(|index| account(&format!("legacy-{index}")))
            .collect::<Vec<_>>();
        pool.import_quotas_legacy(
            accounts
                .iter()
                .zip(cases)
                .map(|(account, quota)| (account_key("anthropic", account), quota)),
        );

        let entries = pool.entries.lock().unwrap();
        let utilization_only = &entries
            .get(&account_key("anthropic", &accounts[0]))
            .unwrap()
            .quota;
        assert!(utilization_only.observed_at_5h.is_some());
        assert_eq!(utilization_only.observed_at_status_5h, None);

        let status_only = &entries
            .get(&account_key("anthropic", &accounts[1]))
            .unwrap()
            .quota;
        assert_eq!(status_only.observed_at_7d, None);
        assert_eq!(status_only.observed_at_status_7d, Some(now - 60));
        assert_eq!(status_only.reset_at_status_7d, Some(reset));

        let both = &entries
            .get(&account_key("anthropic", &accounts[2]))
            .unwrap()
            .quota;
        assert_eq!(both.observed_at_7d_oi, Some(now - 60));
        assert_eq!(both.observed_at_status_7d_oi, Some(now - 60));
        assert_eq!(both.reset_at_status_7d_oi, Some(reset));

        let reset_only = &entries
            .get(&account_key("anthropic", &accounts[3]))
            .unwrap()
            .quota;
        assert_eq!(reset_only.observed_at_5h, None);
        assert_eq!(reset_only.observed_at_status_5h, None);
        assert_eq!(reset_only.reset_at_status_5h, None);

        let signal_free = &entries
            .get(&account_key("anthropic", &accounts[4]))
            .unwrap()
            .quota;
        assert_eq!(signal_free, &QuotaState::default());
    }

    #[test]
    fn legacy_import_preserves_future_reset_after_old_signal_free_timestamp() {
        let now = unix_now();
        let future = now + 3_600;
        let account = account("legacy-reset-only");
        let pool = AccountPool::new();
        pool.import_quotas_legacy([(
            account_key("anthropic", &account),
            QuotaState {
                reset_5h: Some(future),
                observed_at_5h: Some(now - WINDOW_5H_SECS),
                ..Default::default()
            },
        )]);

        let entries = pool.entries.lock().unwrap();
        let quota = &entries
            .get(&account_key("anthropic", &account))
            .unwrap()
            .quota;
        assert_eq!(quota.utilization_5h, None);
        assert_eq!(quota.observed_at_5h, None);
        assert_eq!(quota.reset_5h, Some(future));
    }

    #[test]
    fn legacy_import_matrix_keeps_each_window_owner_shape() {
        let now = unix_now();
        let old = now - 60;
        let future = now + 3_600;
        let mut cases = Vec::new();
        for window in 0..3 {
            for shape in 0..5 {
                let mut quota = QuotaState::default();
                match (window, shape) {
                    (0, 0) => {
                        quota.utilization_5h = Some(0.1);
                        quota.observed_at_5h = Some(old);
                    }
                    (0, 1) => {
                        quota.status_5h = Some("rejected".to_string());
                        quota.observed_at_5h = Some(old);
                        quota.reset_5h = Some(future);
                    }
                    (0, 2) => {
                        quota.utilization_5h = Some(0.1);
                        quota.status_5h = Some("allowed".to_string());
                        quota.observed_at_5h = Some(old);
                        quota.reset_5h = Some(future);
                    }
                    (0, 3) => {
                        quota.observed_at_5h = Some(old);
                        quota.reset_5h = Some(future);
                    }
                    (0, 4) => quota.observed_at_5h = Some(old),
                    (1, 0) => {
                        quota.utilization_7d = Some(0.2);
                        quota.observed_at_7d = Some(old);
                    }
                    (1, 1) => {
                        quota.status_7d = Some("rejected".to_string());
                        quota.observed_at_7d = Some(old);
                        quota.reset_7d = Some(future);
                    }
                    (1, 2) => {
                        quota.utilization_7d = Some(0.2);
                        quota.status_7d = Some("allowed".to_string());
                        quota.observed_at_7d = Some(old);
                        quota.reset_7d = Some(future);
                    }
                    (1, 3) => {
                        quota.observed_at_7d = Some(old);
                        quota.reset_7d = Some(future);
                    }
                    (1, 4) => quota.observed_at_7d = Some(old),
                    (2, 0) => {
                        quota.utilization_7d_oi = Some(0.3);
                        quota.observed_at_7d_oi = Some(old);
                    }
                    (2, 1) => {
                        quota.status_7d_oi = Some("rejected".to_string());
                        quota.observed_at_7d_oi = Some(old);
                        quota.reset_7d_oi = Some(future);
                    }
                    (2, 2) => {
                        quota.utilization_7d_oi = Some(0.3);
                        quota.status_7d_oi = Some("allowed".to_string());
                        quota.observed_at_7d_oi = Some(old);
                        quota.reset_7d_oi = Some(future);
                    }
                    (2, 3) => {
                        quota.observed_at_7d_oi = Some(old);
                        quota.reset_7d_oi = Some(future);
                    }
                    (2, 4) => quota.observed_at_7d_oi = Some(old),
                    _ => unreachable!(),
                }
                cases.push((window, shape, quota));
            }
        }
        let pool = AccountPool::new();
        let accounts = (0..cases.len())
            .map(|index| account(&format!("legacy-matrix-{index}")))
            .collect::<Vec<_>>();
        pool.import_quotas_legacy(
            accounts
                .iter()
                .zip(cases.iter().map(|(_, _, quota)| quota.clone()))
                .map(|(account, quota)| (account_key("anthropic", account), quota)),
        );

        let entries = pool.entries.lock().unwrap();
        for (index, (window, shape, _)) in cases.iter().enumerate() {
            let quota = &entries
                .get(&account_key("anthropic", &accounts[index]))
                .unwrap()
                .quota;
            let (utilization, observed_utilization, status, observed_status, captured_reset) =
                match window {
                    0 => (
                        quota.utilization_5h,
                        quota.observed_at_5h,
                        quota.status_5h.as_deref(),
                        quota.observed_at_status_5h,
                        quota.reset_at_status_5h,
                    ),
                    1 => (
                        quota.utilization_7d,
                        quota.observed_at_7d,
                        quota.status_7d.as_deref(),
                        quota.observed_at_status_7d,
                        quota.reset_at_status_7d,
                    ),
                    2 => (
                        quota.utilization_7d_oi,
                        quota.observed_at_7d_oi,
                        quota.status_7d_oi.as_deref(),
                        quota.observed_at_status_7d_oi,
                        quota.reset_at_status_7d_oi,
                    ),
                    _ => unreachable!(),
                };
            match shape {
                0 => {
                    assert!(utilization.is_some());
                    assert_eq!(observed_utilization, Some(old));
                    assert_eq!(status, None);
                    assert_eq!(observed_status, None);
                }
                1 => {
                    assert_eq!(utilization, None);
                    assert_eq!(observed_utilization, None);
                    assert_eq!(status, Some("rejected"));
                    assert_eq!(observed_status, Some(old));
                    assert_eq!(captured_reset, Some(future));
                }
                2 => {
                    assert!(utilization.is_some());
                    assert_eq!(observed_utilization, Some(old));
                    assert_eq!(status, Some("allowed"));
                    assert_eq!(observed_status, Some(old));
                    assert_eq!(captured_reset, Some(future));
                }
                3 => {
                    assert_eq!(utilization, None);
                    assert_eq!(observed_utilization, None);
                    assert_eq!(status, None);
                    assert_eq!(observed_status, None);
                    assert_eq!(captured_reset, None);
                    assert_eq!(
                        quota.reset_5h.or(quota.reset_7d).or(quota.reset_7d_oi),
                        Some(future)
                    );
                }
                4 => assert_eq!(quota, &QuotaState::default()),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn legacy_aggregate_status_uses_the_earliest_reset_and_expires_at_that_boundary() {
        let now = unix_now();
        let earliest = now + 3_600;
        let later = now + 7_200;
        let mut quota = QuotaState {
            status: Some("rejected".to_string()),
            reset_5h: Some(later),
            reset_7d: Some(earliest),
            reset_7d_oi: Some(later + 3_600),
            ..Default::default()
        };

        assert!(migrate_legacy_status_timestamps(&mut quota, now));
        assert_eq!(
            quota.observed_at_status,
            Some(earliest.saturating_sub(WINDOW_7D_SECS))
        );

        expire_stale_quota(&mut quota, earliest - 1);
        assert_eq!(quota.status.as_deref(), Some("rejected"));
        expire_stale_quota(&mut quota, earliest);
        assert_eq!(quota.status, None);
        assert_eq!(quota.observed_at_status, None);
        assert_eq!(quota.reset_5h, Some(later));
        assert_eq!(quota.reset_7d, None);
    }

    #[test]
    fn legacy_aggregate_status_handles_past_reset_reset_only_and_no_reset() {
        let now = unix_now();

        let past = now.saturating_sub(1);
        let mut past_quota = QuotaState {
            status: Some("rejected".to_string()),
            reset_5h: Some(past),
            ..Default::default()
        };
        migrate_legacy_status_timestamps(&mut past_quota, now);
        expire_stale_quota(&mut past_quota, now);
        assert_eq!(past_quota, QuotaState::default());

        let reset_only = now + 3_600;
        let mut reset_only_quota = QuotaState {
            status: Some("rejected".to_string()),
            reset_7d_oi: Some(reset_only),
            ..Default::default()
        };
        migrate_legacy_status_timestamps(&mut reset_only_quota, now);
        assert_eq!(
            reset_only_quota.observed_at_status,
            Some(reset_only.saturating_sub(WINDOW_7D_SECS))
        );
        assert!(reset_only_quota.utilization_7d_oi.is_none());
        expire_stale_quota(&mut reset_only_quota, reset_only);
        assert_eq!(reset_only_quota.status, None);
        assert_eq!(reset_only_quota.reset_7d_oi, None);

        let mut no_reset_quota = QuotaState {
            status: Some("rejected".to_string()),
            ..Default::default()
        };
        migrate_legacy_status_timestamps(&mut no_reset_quota, now);
        assert_eq!(no_reset_quota.observed_at_status, Some(now));
        expire_stale_quota(&mut no_reset_quota, now + WINDOW_7D_SECS - 1);
        assert_eq!(no_reset_quota.status.as_deref(), Some("rejected"));
        expire_stale_quota(&mut no_reset_quota, now + WINDOW_7D_SECS);
        assert_eq!(no_reset_quota.status, None);
    }

    #[test]
    fn legacy_aggregate_status_clamps_far_future_max_and_epoch_near_resets() {
        let now = unix_now();
        for reset in [now + WINDOW_7D_SECS + 1, u64::MAX] {
            let mut quota = QuotaState {
                status: Some("rejected".to_string()),
                reset_5h: Some(reset),
                ..Default::default()
            };
            migrate_legacy_status_timestamps(&mut quota, now);
            assert_eq!(quota.observed_at_status, Some(now));
            expire_stale_quota(&mut quota, now + WINDOW_7D_SECS - 1);
            assert_eq!(quota.status.as_deref(), Some("rejected"));
            expire_stale_quota(&mut quota, now + WINDOW_7D_SECS);
            assert_eq!(quota.status, None);
            assert_eq!(quota.observed_at_status, None);
            assert_eq!(quota.reset_5h, Some(reset));
        }

        let mut epoch_near = QuotaState {
            status: Some("rejected".to_string()),
            reset_5h: Some(WINDOW_7D_SECS - 1),
            ..Default::default()
        };
        migrate_legacy_status_timestamps(&mut epoch_near, now);
        assert_eq!(epoch_near.observed_at_status, Some(0));
        expire_stale_quota(&mut epoch_near, now);
        assert_eq!(epoch_near, QuotaState::default());
    }

    #[test]
    fn legacy_aggregate_deadline_survives_reset_only_and_usage_updates() {
        let pool = AccountPool::new();
        let account = account("legacy-aggregate-updates");
        let key = account_key("anthropic", &account);
        let now = unix_now();
        let captured_reset = now + 3_600;

        pool.import_quotas_legacy([(
            key.clone(),
            QuotaState {
                status: Some("rejected".to_string()),
                reset_5h: Some(captured_reset),
                ..Default::default()
            },
        )]);
        let captured = pool.raw_quota_for_test(&key).unwrap().1;

        pool.note_quota(
            "anthropic",
            &account,
            &quota_headers(&[(
                "anthropic-ratelimit-unified-5h-reset",
                (captured_reset + 3_600).to_string(),
            )]),
        );
        let after_reset_only = pool.raw_quota_for_test(&key).unwrap().1;
        assert_eq!(
            after_reset_only.observed_at_status, captured.observed_at_status,
            "a reset-only header must not move the migrated aggregate deadline"
        );

        pool.note_usage(
            "anthropic",
            &account,
            &UsageSnapshot {
                five_hour: Some(UsageWindow {
                    utilization: 0.2,
                    resets_at: Some(captured_reset + 7_200),
                }),
                ..Default::default()
            },
        );
        let after_usage = pool.raw_quota_for_test(&key).unwrap().1;
        assert_eq!(
            after_usage.observed_at_status, captured.observed_at_status,
            "a usage update must not move the migrated aggregate deadline"
        );

        let mut swept = after_usage;
        expire_stale_quota(&mut swept, captured_reset);
        assert_eq!(swept.status, None);
        assert_eq!(swept.utilization_5h, Some(0.2));
    }

    #[test]
    fn aggregate_migration_preserves_existing_v2_stamp_and_skips_normal_v3_inference() {
        let now = unix_now();
        let reset = now + 3_600;
        let old_stamp = now.saturating_sub(60);
        let legacy_stamped = account("legacy-stamped");
        let legacy_missing = account("legacy-missing");
        let pool = AccountPool::new();
        pool.import_quotas_legacy([
            (
                account_key("anthropic", &legacy_stamped),
                QuotaState {
                    status: Some("rejected".to_string()),
                    observed_at_status: Some(old_stamp),
                    reset_5h: Some(reset),
                    ..Default::default()
                },
            ),
            (
                account_key("anthropic", &legacy_missing),
                QuotaState {
                    status: Some("rejected".to_string()),
                    reset_5h: Some(reset),
                    ..Default::default()
                },
            ),
        ]);
        assert_eq!(
            pool.raw_quota_for_test(&account_key("anthropic", &legacy_stamped))
                .unwrap()
                .1
                .observed_at_status,
            Some(old_stamp)
        );
        assert_eq!(
            pool.raw_quota_for_test(&account_key("anthropic", &legacy_missing))
                .unwrap()
                .1
                .observed_at_status,
            Some(reset.saturating_sub(WINDOW_7D_SECS))
        );

        let v3_stamped = account("v3-stamped");
        let v3_missing = account("v3-missing");
        let before = unix_now();
        let v3_pool = AccountPool::new();
        v3_pool.import_quotas([
            (
                account_key("anthropic", &v3_stamped),
                QuotaState {
                    status: Some("rejected".to_string()),
                    observed_at_status: Some(old_stamp),
                    reset_5h: Some(reset),
                    ..Default::default()
                },
            ),
            (
                account_key("anthropic", &v3_missing),
                QuotaState {
                    status: Some("rejected".to_string()),
                    reset_5h: Some(reset),
                    ..Default::default()
                },
            ),
        ]);
        assert_eq!(
            v3_pool
                .raw_quota_for_test(&account_key("anthropic", &v3_stamped))
                .unwrap()
                .1
                .observed_at_status,
            Some(old_stamp)
        );
        let v3_missing_stamp = v3_pool
            .raw_quota_for_test(&account_key("anthropic", &v3_missing))
            .unwrap()
            .1
            .observed_at_status
            .expect("normal v3 import stamps missing aggregate status");
        assert!(v3_missing_stamp >= before);
        assert_ne!(
            v3_missing_stamp,
            reset.saturating_sub(WINDOW_7D_SECS),
            "normal v3 import must not infer a timestamp from reset metadata"
        );
    }

    #[test]
    fn aggregate_migration_is_independent_per_account_and_shared_by_aliases() {
        let now = unix_now();
        let first = account_with_uuid("first", "physical-first");
        let first_alias = account_with_uuid("first-alias", "physical-first");
        let second = account_with_uuid("second", "physical-second");
        let first_reset = now + 3_600;
        let second_reset = now + 7_200;
        let pool = AccountPool::new();
        pool.import_quotas_legacy([
            (
                account_key("anthropic", &first),
                QuotaState {
                    status: Some("rejected".to_string()),
                    reset_5h: Some(first_reset),
                    ..Default::default()
                },
            ),
            (
                account_key("anthropic", &second),
                QuotaState {
                    status: Some("rejected".to_string()),
                    reset_5h: Some(second_reset),
                    ..Default::default()
                },
            ),
        ]);

        let first_key = account_key("anthropic", &first);
        let alias_key = account_key("anthropic", &first_alias);
        let second_key = account_key("anthropic", &second);
        assert_eq!(first_key, alias_key);
        assert_eq!(
            pool.raw_quota_for_test(&first_key)
                .unwrap()
                .1
                .observed_at_status,
            Some(first_reset.saturating_sub(WINDOW_7D_SECS))
        );
        assert_eq!(
            pool.raw_quota_for_test(&second_key)
                .unwrap()
                .1
                .observed_at_status,
            Some(second_reset.saturating_sub(WINDOW_7D_SECS))
        );

        pool.note_usage(
            "anthropic",
            &first_alias,
            &UsageSnapshot {
                five_hour: Some(UsageWindow {
                    utilization: 0.3,
                    resets_at: Some(first_reset + 3_600),
                }),
                ..Default::default()
            },
        );
        assert_eq!(
            pool.raw_quota_for_test(&alias_key)
                .unwrap()
                .1
                .observed_at_status,
            Some(first_reset.saturating_sub(WINDOW_7D_SECS))
        );
        assert_eq!(
            pool.raw_quota_for_test(&second_key)
                .unwrap()
                .1
                .observed_at_status,
            Some(second_reset.saturating_sub(WINDOW_7D_SECS))
        );
    }

    #[test]
    fn note_quota_records_each_window_status_when_present() {
        let pool = AccountPool::new();
        let account = account("a");
        pool.note_quota(
            "anthropic",
            &account,
            &quota_headers(&[
                (QUOTA_STATUS_HEADERS[0], "allowed".to_string()),
                (QUOTA_STATUS_HEADERS[1], "rejected".to_string()),
                (QUOTA_STATUS_HEADERS[2], "allowed".to_string()),
            ]),
        );
        let entries = pool.entries.lock().unwrap();
        let quota = &entries
            .get(&account_key("anthropic", &account))
            .unwrap()
            .quota;
        assert_eq!(quota.status_5h.as_deref(), Some("allowed"));
        assert_eq!(quota.status_7d.as_deref(), Some("rejected"));
        assert_eq!(quota.status_7d_oi.as_deref(), Some("allowed"));
        assert!(quota.observed_at_status_5h.is_some());
        assert!(quota.observed_at_status_7d.is_some());
        assert!(quota.observed_at_status_7d_oi.is_some());
        assert_eq!(quota.reset_at_status_5h, None);
        assert_eq!(quota.reset_at_status_7d, None);
        assert_eq!(quota.reset_at_status_7d_oi, None);
    }

    #[test]
    fn usage_refreshes_only_utilization_freshness_and_keeps_status_boundaries() {
        let pool = AccountPool::new();
        let account = account("status-boundaries");
        let now = unix_now();
        let reset_5h = now + 300;
        let reset_7d = now + 600;
        let reset_7d_oi = now + 900;
        pool.note_quota(
            "anthropic",
            &account,
            &quota_headers(&[
                (QUOTA_STATUS_HEADERS[0], "rejected".to_string()),
                (QUOTA_STATUS_HEADERS[1], "allowed".to_string()),
                (QUOTA_STATUS_HEADERS[2], "rejected".to_string()),
                ("anthropic-ratelimit-unified-5h-reset", reset_5h.to_string()),
                ("anthropic-ratelimit-unified-7d-reset", reset_7d.to_string()),
                (
                    "anthropic-ratelimit-unified-7d_oi-reset",
                    reset_7d_oi.to_string(),
                ),
            ]),
        );
        let key = account_key("anthropic", &account);
        let before = pool.raw_quota_for_test(&key).unwrap().1;

        pool.note_usage(
            "anthropic",
            &account,
            &UsageSnapshot {
                five_hour: Some(UsageWindow {
                    utilization: 0.1,
                    resets_at: Some(now + 1_200),
                }),
                seven_day: Some(UsageWindow {
                    utilization: 0.2,
                    resets_at: Some(now + 1_500),
                }),
                seven_day_oi: Some(UsageWindow {
                    utilization: 0.3,
                    resets_at: Some(now + 1_800),
                }),
            },
        );

        let after = pool.raw_quota_for_test(&key).unwrap().1;
        assert_eq!(after.observed_at_status_5h, before.observed_at_status_5h);
        assert_eq!(after.observed_at_status_7d, before.observed_at_status_7d);
        assert_eq!(
            after.observed_at_status_7d_oi,
            before.observed_at_status_7d_oi
        );
        assert_eq!(after.reset_at_status_5h, Some(reset_5h));
        assert_eq!(after.reset_at_status_7d, Some(reset_7d));
        assert_eq!(after.reset_at_status_7d_oi, Some(reset_7d_oi));
        assert_eq!(after.utilization_5h, Some(0.1));
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
        assert_ne!(
            selected[0], sticky,
            "a fresh aggregate rejection remains near after the 5h window expires"
        );
        let entries = pool.entries.lock().unwrap();
        let quota = &entries
            .get(&account_key("anthropic", &accounts[sticky]))
            .unwrap()
            .quota;
        assert_eq!(quota.utilization_5h, None);
        assert_eq!(quota.reset_5h, None);
        assert_eq!(quota.utilization_7d, Some(0.42));
        assert_eq!(quota.reset_7d, None);
        assert_eq!(quota.status.as_deref(), Some("rejected"));
        assert!(quota.observed_at_status.is_some());
    }

    #[test]
    fn note_quota_stamps_observation_per_window_and_aggregate() {
        // Utilization-only, status-only, and aggregate-only writes each stamp
        // their own observation time independently of one another.
        let pool = AccountPool::new();
        let account = account("a");
        let before = unix_now();

        pool.note_quota(
            "anthropic",
            &account,
            &quota_headers(&[(
                "anthropic-ratelimit-unified-5h-utilization",
                "0.3".to_string(),
            )]),
        );
        {
            let entries = pool.entries.lock().unwrap();
            let quota = &entries
                .get(&account_key("anthropic", &account))
                .unwrap()
                .quota;
            assert!(quota.observed_at_5h.is_some_and(|at| at >= before));
            assert!(quota.observed_at_7d.is_none());
            assert!(quota.observed_at_status_5h.is_none());
            assert!(quota.observed_at_status.is_none());
        }

        pool.note_quota(
            "anthropic",
            &account,
            &quota_headers(&[(QUOTA_STATUS_HEADERS[1], "rejected".to_string())]),
        );
        {
            let entries = pool.entries.lock().unwrap();
            let quota = &entries
                .get(&account_key("anthropic", &account))
                .unwrap()
                .quota;
            assert!(quota.observed_at_status_7d.is_some_and(|at| at >= before));
            assert!(quota.observed_at_7d.is_none());
            assert!(quota.observed_at_status.is_none());
        }

        pool.note_quota(
            "anthropic",
            &account,
            &quota_headers(&[("anthropic-ratelimit-unified-status", "rejected".to_string())]),
        );
        {
            let entries = pool.entries.lock().unwrap();
            let quota = &entries
                .get(&account_key("anthropic", &account))
                .unwrap()
                .quota;
            assert!(quota.observed_at_status.is_some_and(|at| at >= before));
        }
    }

    #[test]
    fn anthropic_header_inputs_sweep_a_passed_window_before_replacement() {
        let now = unix_now();
        let future = now + 3_600;
        let stale = now.saturating_sub(1);

        let pool = AccountPool::new();
        let utilization_only = account("header-utilization");
        pool.note_quota(
            "anthropic",
            &utilization_only,
            &quota_headers(&[
                (QUOTA_STATUS_HEADERS[0], "rejected".to_string()),
                (
                    "anthropic-ratelimit-unified-5h-utilization",
                    "0.4".to_string(),
                ),
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    (now + 1_000).to_string(),
                ),
            ]),
        );
        {
            let mut entries = pool.entries.lock().unwrap();
            let quota = &mut entries
                .get_mut(&account_key("anthropic", &utilization_only))
                .unwrap()
                .quota;
            quota.reset_5h = Some(stale);
            quota.reset_at_status_5h = Some(stale);
        }
        pool.note_quota(
            "anthropic",
            &utilization_only,
            &quota_headers(&[
                (
                    "anthropic-ratelimit-unified-5h-utilization",
                    "0.2".to_string(),
                ),
                ("anthropic-ratelimit-unified-5h-reset", future.to_string()),
            ]),
        );
        let quota = pool
            .raw_quota_for_test(&account_key("anthropic", &utilization_only))
            .unwrap()
            .1;
        assert_eq!(quota.status_5h, None);
        assert_eq!(quota.reset_at_status_5h, None);
        assert_eq!(quota.utilization_5h, Some(0.2));
        assert_eq!(quota.reset_5h, Some(future));

        let status_only = account("header-status");
        pool.note_quota(
            "anthropic",
            &status_only,
            &quota_headers(&[
                (
                    "anthropic-ratelimit-unified-5h-utilization",
                    "0.4".to_string(),
                ),
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    (now + 1_000).to_string(),
                ),
            ]),
        );
        {
            let mut entries = pool.entries.lock().unwrap();
            let quota = &mut entries
                .get_mut(&account_key("anthropic", &status_only))
                .unwrap()
                .quota;
            quota.reset_5h = Some(stale);
        }
        pool.note_quota(
            "anthropic",
            &status_only,
            &quota_headers(&[
                (QUOTA_STATUS_HEADERS[0], "allowed".to_string()),
                ("anthropic-ratelimit-unified-5h-reset", future.to_string()),
            ]),
        );
        let quota = pool
            .raw_quota_for_test(&account_key("anthropic", &status_only))
            .unwrap()
            .1;
        assert_eq!(quota.utilization_5h, None);
        assert_eq!(quota.status_5h.as_deref(), Some("allowed"));
        assert_eq!(quota.reset_at_status_5h, Some(future));

        let reset_only = account("header-reset-only");
        pool.note_quota(
            "anthropic",
            &reset_only,
            &quota_headers(&[
                (QUOTA_STATUS_HEADERS[0], "rejected".to_string()),
                (
                    "anthropic-ratelimit-unified-5h-utilization",
                    "0.4".to_string(),
                ),
                (
                    "anthropic-ratelimit-unified-5h-reset",
                    (now + 1_000).to_string(),
                ),
            ]),
        );
        {
            let mut entries = pool.entries.lock().unwrap();
            let quota = &mut entries
                .get_mut(&account_key("anthropic", &reset_only))
                .unwrap()
                .quota;
            quota.reset_5h = Some(stale);
            quota.reset_at_status_5h = Some(stale);
        }
        pool.note_quota(
            "anthropic",
            &reset_only,
            &quota_headers(&[("anthropic-ratelimit-unified-5h-reset", future.to_string())]),
        );
        let quota = pool
            .raw_quota_for_test(&account_key("anthropic", &reset_only))
            .unwrap()
            .1;
        assert_eq!(quota.utilization_5h, None);
        assert_eq!(quota.status_5h, None);
        assert_eq!(quota.observed_at_5h, None);
        assert_eq!(quota.observed_at_status_5h, None);
        assert_eq!(quota.reset_5h, Some(future));
        assert_eq!(quota.reset_at_status_5h, None);
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
            .get(&account_key("anthropic", &accounts[0]))
            .unwrap()
            .quota;
        assert_eq!(quota.utilization_5h, Some(0.1));
        assert_eq!(quota.utilization_7d, Some(0.2));
        assert_eq!(quota.utilization_7d_oi, Some(0.5));
    }

    #[test]
    fn note_usage_stamps_observation() {
        let pool = AccountPool::new();
        let accounts = [account("a")];
        let before = unix_now();
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
                seven_day_oi: Some(UsageWindow {
                    utilization: 0.3,
                    resets_at: None,
                }),
            },
        );
        let entries = pool.entries.lock().unwrap();
        let quota = &entries
            .get(&account_key("anthropic", &accounts[0]))
            .unwrap()
            .quota;
        assert!(quota.observed_at_5h.is_some_and(|at| at >= before));
        assert!(quota.observed_at_7d.is_some_and(|at| at >= before));
        assert!(quota.observed_at_7d_oi.is_some_and(|at| at >= before));
    }

    #[test]
    fn usage_only_poll_preserves_per_window_status_freshness() {
        let pool = AccountPool::new();
        let account = account("status-freshness");
        pool.note_quota(
            "anthropic",
            &account,
            &quota_headers(&[(QUOTA_STATUS_HEADERS[0], "rejected".to_string())]),
        );
        let key = account_key("anthropic", &account);
        let status_observed_at = unix_now().saturating_sub(60);
        {
            let mut entries = pool.entries.lock().unwrap();
            entries
                .get_mut(&key)
                .expect("status observation was recorded")
                .quota
                .observed_at_status_5h = Some(status_observed_at);
        }

        pool.note_usage(
            "anthropic",
            &account,
            &UsageSnapshot {
                five_hour: Some(UsageWindow {
                    utilization: 0.1,
                    resets_at: None,
                }),
                seven_day: None,
                seven_day_oi: None,
            },
        );

        let quota = pool.raw_quota_for_test(&key).unwrap().1;
        assert_eq!(quota.observed_at_status_5h, Some(status_observed_at));
        assert!(quota
            .observed_at_5h
            .is_some_and(|at| at >= status_observed_at));
    }

    #[test]
    fn usage_without_reset_preserves_header_reset() {
        let pool = AccountPool::new();
        let accounts = [account("a")];
        let future = unix_now() + 3_600;
        // A prior header records a future 5h reset.
        pool.note_quota(
            "anthropic",
            &accounts[0],
            &quota_headers(&[
                (
                    "anthropic-ratelimit-unified-5h-utilization",
                    "0.5".to_string(),
                ),
                ("anthropic-ratelimit-unified-5h-reset", future.to_string()),
            ]),
        );
        // The usage poll applies without a resets_at — the future header reset
        // must survive.
        pool.note_usage(
            "anthropic",
            &accounts[0],
            &UsageSnapshot {
                five_hour: Some(UsageWindow {
                    utilization: 0.6,
                    resets_at: None,
                }),
                seven_day: None,
                seven_day_oi: None,
            },
        );
        let entries = pool.entries.lock().unwrap();
        let quota = &entries
            .get(&account_key("anthropic", &accounts[0]))
            .unwrap()
            .quota;
        assert_eq!(quota.utilization_5h, Some(0.6));
        assert_eq!(quota.reset_5h, Some(future));
    }

    #[test]
    fn usage_without_reset_clears_past_stored_reset() {
        // Revision-3 fix (P2-3): a past stored reset must not survive an
        // omitted resets_at, or the next expire_stale_quota sweep would erase
        // the utilization this same call just wrote, and the next poll would
        // write it right back — an indefinite write/expire/rewrite cycle.
        let pool = AccountPool::new();
        let accounts = [account("a")];
        let past = unix_now().saturating_sub(1);
        pool.note_quota(
            "anthropic",
            &accounts[0],
            &quota_headers(&[
                (
                    "anthropic-ratelimit-unified-7d-utilization",
                    "0.5".to_string(),
                ),
                ("anthropic-ratelimit-unified-7d-reset", past.to_string()),
            ]),
        );
        pool.note_usage(
            "anthropic",
            &accounts[0],
            &UsageSnapshot {
                five_hour: None,
                seven_day: Some(UsageWindow {
                    utilization: 0.7,
                    resets_at: None,
                }),
                seven_day_oi: None,
            },
        );
        let entries = pool.entries.lock().unwrap();
        let quota = &entries
            .get(&account_key("anthropic", &accounts[0]))
            .unwrap()
            .quota;
        assert_eq!(quota.utilization_7d, Some(0.7));
        assert_eq!(quota.reset_7d, None);
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
    fn classifies_upstream_responses_kimi() {
        // classify_kimi mirrors classify except for the added 402 case: a
        // Kimi account with an inactive subscription membership returns 402
        // on every request, so it must rotate rather than relay.
        assert_eq!(
            classify_kimi(StatusCode::PAYMENT_REQUIRED, &HeaderMap::new()),
            FailoverAction::Rotate
        );
        assert_eq!(
            classify_kimi(StatusCode::TOO_MANY_REQUESTS, &HeaderMap::new()),
            FailoverAction::PauseSame
        );
        assert_eq!(
            classify_kimi(StatusCode::UNAUTHORIZED, &HeaderMap::new()),
            FailoverAction::RefreshRetry
        );
        assert_eq!(
            classify_kimi(StatusCode::SERVICE_UNAVAILABLE, &HeaderMap::new()),
            FailoverAction::Rotate
        );
        assert_eq!(
            classify_kimi(StatusCode::OK, &HeaderMap::new()),
            FailoverAction::Relay
        );
        assert_eq!(
            classify_kimi(StatusCode::BAD_REQUEST, &HeaderMap::new()),
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
    fn observation_time_never_feeds_headroom() {
        // F2 (a reset synthesized from observed_at) was rejected during design
        // because it would corrupt window_headroom's burn-rate math.
        // observed_at_5h must have zero effect on headroom or `near`: this
        // holds reset at None and utilization under threshold, with
        // burn_rate_avoidance on, so any headroom leak from observed_at would
        // flip `near` to true.
        let quota = QuotaState {
            utilization_5h: Some(0.3),
            reset_5h: None,
            observed_at_5h: Some(unix_now().saturating_sub(60)),
            ..Default::default()
        };
        let pool_cfg = PoolConfig {
            burn_rate_avoidance: true,
            ..Default::default()
        };
        let assessment = assess_quota(&quota, &account("a"), false, Some(&pool_cfg), unix_now());
        assert!(
            !assessment.near,
            "a reset-less window under threshold is never near, regardless of observed_at"
        );
        assert_eq!(
            assessment.headroom,
            f64::INFINITY,
            "observed_at must not feed headroom"
        );
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
        assert_eq!(exported[0].0, account_key("anthropic", &acct));

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
            account_key("anthropic", &account("a")),
            QuotaState {
                status: Some("rejected".to_string()),
                ..Default::default()
            },
        )]);

        let exported = pool.export_quotas();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].1.status.as_deref(), Some("rejected"));

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
            account_key("anthropic", &account("a")),
            QuotaState {
                reset_7d: Some(reset),
                ..Default::default()
            },
        )]);

        let exported = pool.export_quotas();
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].1.reset_7d, Some(reset));

        let restored = AccountPool::new();
        restored.import_quotas(exported);
        let snapshots = restored.snapshot("anthropic", &[account("a")], None, None);
        assert_eq!(snapshots[0].reset_7d, Some(reset));
    }

    #[test]
    fn import_stamps_observation_for_legacy_reset_less_quota() {
        // A state file written before observed_at_* existed has a window and
        // an aggregate signal but no observation timestamps at all. Importing
        // it must backdate both to boot time rather than leave them None,
        // which for a reset-less mark would mean "expire immediately" on the
        // very next sweep — the opposite of the intended warm start.
        let before = unix_now();
        let pool = AccountPool::new();
        pool.import_quotas([(
            account_key("anthropic", &account("a")),
            QuotaState {
                utilization_7d: Some(0.9),
                status: Some("rejected".to_string()),
                ..Default::default()
            },
        )]);

        let entries = pool.entries.lock().unwrap();
        let quota = &entries
            .get(&account_key("anthropic", &account("a")))
            .unwrap()
            .quota;
        assert!(quota.observed_at_7d.is_some_and(|at| at >= before));
        assert!(quota.observed_at_status.is_some_and(|at| at >= before));
    }

    #[test]
    fn import_sweeps_expired_legacy_aggregate_before_stamping() {
        let account = account("expired-legacy");
        let pool = AccountPool::new();
        let corrected = pool.import_quotas([(
            account_key("anthropic", &account),
            QuotaState {
                utilization_5h: Some(0.9),
                reset_5h: Some(unix_now().saturating_sub(1)),
                status: Some("rejected".to_string()),
                ..Default::default()
            },
        )]);

        assert!(corrected, "import reports the expired quota mutation");
        let entries = pool.entries.lock().unwrap();
        let health = entries
            .get(&account_key("anthropic", &account))
            .expect("restored account exists");
        assert!(health.observed);
        assert_eq!(health.quota, QuotaState::default());
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
        let mut stored = account("a");
        stored.store_entry = true;
        stored.store_family = Some(StoreFamily::Claude);
        pool.import_quotas([(
            account_key("anthropic", &stored),
            QuotaState {
                utilization_5h: Some(0.5),
                ..Default::default()
            },
        )]);
        assert!(!pool.take_dirty(), "restored quota starts clean");
        assert_eq!(pool.export_quotas().len(), 1);

        pool.forget_identity(StoreFamily::Claude, "a");

        assert!(pool.take_dirty(), "removing persisted quota marks dirty");
        assert!(pool.export_quotas().is_empty());
    }

    #[test]
    fn forgetting_cooldown_only_state_does_not_mark_dirty() {
        let pool = AccountPool::new();
        let mut stored = account("a");
        stored.store_entry = true;
        stored.store_family = Some(StoreFamily::Claude);
        pool.cooldown("anthropic", &stored, Duration::from_secs(60), "test");

        pool.forget_identity(StoreFamily::Claude, "a");

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

    /// Stamp a near-quota utilization (0.9, comfortably under the hard 0.98
    /// backstop but past the 0.5 `default_threshold` these tests use) with an
    /// explicit `observed_at_5h`, bypassing the header-parsing path so the
    /// exact observation time is under test control. Used by the
    /// opportunistic re-probe (Change B) tests below.
    fn stamp_near_quota(
        pool: &AccountPool,
        provider: &str,
        account: &AccountConfig,
        observed_at: u64,
    ) {
        let mut entries = pool.entries.lock().expect("account health lock poisoned");
        let health = entries.entry(account_key(provider, account)).or_default();
        health.quota.utilization_5h = Some(0.9);
        health.quota.observed_at_5h = Some(observed_at);
    }

    #[test]
    fn reprobe_interval_boundaries() {
        let cases = [
            ("pool absent", None, None),
            ("zero", Some(Some(0)), None),
            ("unset", Some(None), Some(REPROBE_DEFAULT_SECS)),
            ("one", Some(Some(1)), Some(REPROBE_FLOOR_SECS)),
            ("below floor", Some(Some(59)), Some(REPROBE_FLOOR_SECS)),
            ("at floor", Some(Some(60)), Some(REPROBE_FLOOR_SECS)),
            ("above floor", Some(Some(61)), Some(61)),
        ];

        for (label, configured, expected_seconds) in cases {
            let pool = configured.map(|reprobe_seconds| PoolConfig {
                reprobe_seconds,
                ..Default::default()
            });
            assert_eq!(
                reprobe_interval(pool.as_ref()),
                expected_seconds.map(Duration::from_secs),
                "{label}"
            );
        }
    }

    #[test]
    fn stale_near_codex_account_probes_ahead_once() {
        let pool_cfg = PoolConfig {
            default_threshold: Some(0.5),
            reprobe_seconds: Some(60),
            ..Default::default()
        };
        let pool = Arc::new(AccountPool::new());
        let mut a = account("codex-a");
        a.store_family = Some(StoreFamily::Chatgpt);
        let mut b = account("codex-b");
        b.store_family = Some(StoreFamily::Chatgpt);
        let accounts = vec![a, b];
        let session = "reprobe-once";
        let initial = pool.select_order("codex", &accounts, Some(session), None, Some(&pool_cfg));
        let stale = initial[0];
        stamp_near_quota(&pool, "codex", &accounts[stale], unix_now() - 61);

        let (order, reservation) =
            pool.select_order_deferred("codex", &accounts, Some(session), None, Some(&pool_cfg));
        assert_eq!(
            order[0], stale,
            "a stale near-quota account is promoted to the front for one probe"
        );
        assert!(reservation.is_some(), "promotion must carry a reservation");

        let (order, second_reservation) =
            pool.select_order_deferred("codex", &accounts, Some(session), None, Some(&pool_cfg));
        assert_ne!(
            order[0], stale,
            "the same account does not reserve again while the first dispatch is pending"
        );
        assert!(second_reservation.is_none());
        drop(reservation);
    }

    #[test]
    fn reprobe_reservation_commit_cancel_and_drop_are_single_use() {
        let pool_cfg = PoolConfig {
            default_threshold: Some(0.5),
            reprobe_seconds: Some(60),
            ..Default::default()
        };
        let accounts = || {
            let mut stale = account("reprobe-stale");
            stale.store_family = Some(StoreFamily::Chatgpt);
            let mut healthy = account("reprobe-healthy");
            healthy.store_family = Some(StoreFamily::Chatgpt);
            vec![stale, healthy]
        };

        let commit_provider = "reprobe-lifecycle-commit";
        let commit_pool = Arc::new(AccountPool::new());
        let commit_accounts = accounts();
        let initial = commit_pool.select_order(
            commit_provider,
            &commit_accounts,
            Some("reprobe-lifecycle-commit-session"),
            None,
            Some(&pool_cfg),
        );
        let stale = initial[0];
        let observed_at = unix_now() - 61;
        stamp_near_quota(
            &commit_pool,
            commit_provider,
            &commit_accounts[stale],
            observed_at,
        );
        let (order, reservation) = commit_pool.select_order_deferred(
            commit_provider,
            &commit_accounts,
            Some("reprobe-lifecycle-commit-session"),
            None,
            Some(&pool_cfg),
        );
        assert_eq!(order[0], stale);
        let mut reservation = reservation.expect("stale selection reserves once");
        assert_eq!(
            commit_pool.last_probe_at_for_test(commit_provider, &commit_accounts[stale]),
            None,
            "selection alone must not stamp dispatch time"
        );
        let before = crate::metrics::pool_reprobe_count_for_tests(commit_provider);
        assert!(reservation.commit());
        assert!(!reservation.commit(), "a reservation commits at most once");
        assert!(commit_pool
            .last_probe_at_for_test(commit_provider, &commit_accounts[stale])
            .is_some());
        assert_eq!(
            crate::metrics::pool_reprobe_count_for_tests(commit_provider),
            before + 1,
            "one committed dispatch records one provider counter"
        );
        let committed_quota = commit_pool
            .entries
            .lock()
            .expect("account health lock poisoned")
            .get(&account_key(commit_provider, &commit_accounts[stale]))
            .expect("committed account health remains observable")
            .quota
            .clone();
        assert_eq!(
            committed_quota.observed_at_5h,
            Some(observed_at),
            "committing a reprobe must not rewrite quota observation time"
        );
        drop(reservation);

        let cancel_provider = "reprobe-lifecycle-cancel";
        let cancel_pool = Arc::new(AccountPool::new());
        let cancel_accounts = accounts();
        let initial = cancel_pool.select_order(
            cancel_provider,
            &cancel_accounts,
            Some("reprobe-lifecycle-cancel-session"),
            None,
            Some(&pool_cfg),
        );
        let stale = initial[0];
        stamp_near_quota(
            &cancel_pool,
            cancel_provider,
            &cancel_accounts[stale],
            unix_now() - 61,
        );
        let (_, reservation) = cancel_pool.select_order_deferred(
            cancel_provider,
            &cancel_accounts,
            Some("reprobe-lifecycle-cancel-session"),
            None,
            Some(&pool_cfg),
        );
        let mut reservation = reservation.expect("stale selection reserves once");
        let before = crate::metrics::pool_reprobe_count_for_tests(cancel_provider);
        reservation.cancel();
        assert!(
            !reservation.commit(),
            "a cancelled reservation cannot commit"
        );
        assert_eq!(
            cancel_pool.last_probe_at_for_test(cancel_provider, &cancel_accounts[stale]),
            None
        );
        assert_eq!(
            crate::metrics::pool_reprobe_count_for_tests(cancel_provider),
            before,
            "cancellation does not record a dispatch"
        );
        drop(reservation);
        let (_, retry) = cancel_pool.select_order_deferred(
            cancel_provider,
            &cancel_accounts,
            Some("reprobe-lifecycle-cancel-session"),
            None,
            Some(&pool_cfg),
        );
        assert!(
            retry.is_some(),
            "cancelling a reservation makes the stale account immediately eligible again"
        );
        drop(retry);

        let drop_provider = "reprobe-lifecycle-drop";
        let drop_pool = Arc::new(AccountPool::new());
        let drop_accounts = accounts();
        let initial = drop_pool.select_order(
            drop_provider,
            &drop_accounts,
            Some("reprobe-lifecycle-drop-session"),
            None,
            Some(&pool_cfg),
        );
        let stale = initial[0];
        stamp_near_quota(
            &drop_pool,
            drop_provider,
            &drop_accounts[stale],
            unix_now() - 61,
        );
        let (order, reservation) = drop_pool.select_order_deferred(
            drop_provider,
            &drop_accounts,
            Some("reprobe-lifecycle-drop-session"),
            None,
            Some(&pool_cfg),
        );
        assert_eq!(order[0], stale);
        drop(reservation);
        assert_eq!(
            drop_pool.last_probe_at_for_test(drop_provider, &drop_accounts[stale]),
            None,
            "dropping before dispatch cancels the reservation"
        );
        let (_, retry) = drop_pool.select_order_deferred(
            drop_provider,
            &drop_accounts,
            Some("reprobe-lifecycle-drop-session"),
            None,
            Some(&pool_cfg),
        );
        assert!(
            retry.is_some(),
            "a dropped reservation leaves the account eligible"
        );
        drop(retry);
    }

    #[test]
    fn fresh_near_observation_suppresses_probe() {
        let pool_cfg = PoolConfig {
            default_threshold: Some(0.5),
            reprobe_seconds: Some(60),
            ..Default::default()
        };
        let pool = AccountPool::new();
        let mut a = account("codex-a");
        a.store_family = Some(StoreFamily::Chatgpt);
        let mut b = account("codex-b");
        b.store_family = Some(StoreFamily::Chatgpt);
        let accounts = vec![a, b];
        let session = "fresh-observation";
        let initial = pool.select_order("codex", &accounts, Some(session), None, Some(&pool_cfg));
        let sticky = initial[0];
        let other = initial[1];
        stamp_near_quota(&pool, "codex", &accounts[sticky], unix_now());

        let order = pool.select_order("codex", &accounts, Some(session), None, Some(&pool_cfg));
        assert_eq!(
            order,
            vec![other, sticky],
            "a freshly observed near-quota account sorts normally, with no promotion"
        );
    }

    #[test]
    fn fresh_per_window_status_rejection_suppresses_probe() {
        let pool_cfg = PoolConfig {
            default_threshold: Some(0.5),
            reprobe_seconds: Some(60),
            ..Default::default()
        };
        let pool = Arc::new(AccountPool::new());
        let mut a = account("codex-a");
        a.store_family = Some(StoreFamily::Chatgpt);
        let mut b = account("codex-b");
        b.store_family = Some(StoreFamily::Chatgpt);
        let accounts = vec![a, b];
        let session = "per-window-status-rejection";
        let initial = pool.select_order("codex", &accounts, Some(session), None, Some(&pool_cfg));
        let sticky = initial[0];
        let other = initial[1];

        {
            let mut entries = pool.entries.lock().expect("account health lock poisoned");
            let health = entries
                .entry(account_key("codex", &accounts[sticky]))
                .or_default();
            health.quota.status_5h = Some("rejected".to_string());
            health.quota.observed_at_status_5h = Some(unix_now());
        }

        let (order, reservation) =
            pool.select_order_deferred("codex", &accounts, Some(session), None, Some(&pool_cfg));
        assert_eq!(order, vec![other, sticky]);
        assert!(
            reservation.is_none(),
            "a fresh 5h status must suppress probing"
        );
    }

    #[test]
    fn recent_aggregate_rejection_suppresses_probe() {
        let pool_cfg = PoolConfig {
            default_threshold: Some(0.5),
            reprobe_seconds: Some(60),
            ..Default::default()
        };
        let pool = Arc::new(AccountPool::new());
        let mut a = account("codex-a");
        a.store_family = Some(StoreFamily::Chatgpt);
        let mut b = account("codex-b");
        b.store_family = Some(StoreFamily::Chatgpt);
        let accounts = vec![a, b];
        let session = "aggregate-rejection";
        let initial = pool.select_order("codex", &accounts, Some(session), None, Some(&pool_cfg));
        let sticky = initial[0];
        let other = initial[1];

        // Only the aggregate `status` is set (no per-window utilization or
        // status), so `assess_quota`'s `has_window_status` fallback is what
        // marks this account near -- and `observed_at_status` alone must
        // govern this candidate's freshness.
        {
            let mut entries = pool.entries.lock().expect("account health lock poisoned");
            let health = entries
                .entry(account_key("codex", &accounts[sticky]))
                .or_default();
            health.quota.status = Some("rejected".to_string());
            health.quota.observed_at_status = Some(unix_now());
        }
        let order = pool.select_order("codex", &accounts, Some(session), None, Some(&pool_cfg));
        assert_eq!(
            order,
            vec![other, sticky],
            "a freshly rejected aggregate status is not probed yet"
        );

        {
            let mut entries = pool.entries.lock().expect("account health lock poisoned");
            let health = entries
                .get_mut(&account_key("codex", &accounts[sticky]))
                .unwrap();
            health.quota.observed_at_status = Some(unix_now() - 61);
        }
        let (order, reservation) =
            pool.select_order_deferred("codex", &accounts, Some(session), None, Some(&pool_cfg));
        assert_eq!(
            order[0], sticky,
            "the stamp aging past the interval unblocks the probe"
        );
        assert!(reservation.is_some());
    }

    /// Concurrent deferred selections reserve the same stale account at most
    /// once while the first request is waiting to dispatch.
    #[test]
    fn concurrent_stale_probe_selection_never_double_promotes() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Barrier,
        };

        let pool_cfg = PoolConfig {
            default_threshold: Some(0.5),
            reprobe_seconds: Some(60),
            ..Default::default()
        };
        let pool = Arc::new(AccountPool::new());
        let mut a = account("codex-a");
        a.store_family = Some(StoreFamily::Chatgpt);
        let mut b = account("codex-b");
        b.store_family = Some(StoreFamily::Chatgpt);
        let accounts = vec![a, b];
        let session = "concurrent-probe-race";

        let initial = pool.select_order("codex", &accounts, Some(session), None, Some(&pool_cfg));
        let stale = initial[0];
        // Keep the observation inside the 5-hour lifetime bound while making
        // it older than the configured reprobe interval.
        stamp_near_quota(&pool, "codex", &accounts[stale], unix_now() - 61);

        const ROUNDS: usize = 200;
        const RACERS: usize = 8;
        for round in 0..ROUNDS {
            {
                let mut entries = pool.entries.lock().expect("account health lock poisoned");
                let health = entries
                    .get_mut(&account_key("codex", &accounts[stale]))
                    .expect("stamp_near_quota above already inserted this entry");
                health.last_probe_at = None;
                health.reprobe_reservation = None;
            }
            let promotions = AtomicUsize::new(0);
            let barrier = Arc::new(Barrier::new(RACERS + 1));
            std::thread::scope(|scope| {
                for _ in 0..RACERS {
                    let pool = &pool;
                    let accounts = &accounts;
                    let pool_cfg = &pool_cfg;
                    let promotions = &promotions;
                    let barrier = Arc::clone(&barrier);
                    scope.spawn(move || {
                        let (order, reservation) = pool.select_order_deferred(
                            "codex",
                            accounts,
                            Some(session),
                            None,
                            Some(pool_cfg),
                        );
                        if reservation.is_some() && order[0] == stale {
                            promotions.fetch_add(1, Ordering::SeqCst);
                        }
                        barrier.wait();
                        drop(reservation);
                    });
                }
                barrier.wait();
            });
            assert_eq!(
                promotions.load(Ordering::SeqCst),
                1,
                "round {round}: exactly one concurrent select_order call must promote \
                 the stale account -- select and stamp happen inside the same lock \
                 acquisition, so no other racing call in this round can still observe \
                 it unprobed"
            );
        }
    }

    #[test]
    fn claude_family_accounts_are_never_probed() {
        let pool_cfg = PoolConfig {
            default_threshold: Some(0.5),
            reprobe_seconds: Some(60),
            ..Default::default()
        };
        let pool = AccountPool::new();
        let mut a = account("claude-a");
        a.store_family = Some(StoreFamily::Claude);
        let mut b = account("claude-b");
        b.store_family = Some(StoreFamily::Claude);
        let accounts = vec![a, b];
        let session = "claude-never-probed";
        let initial =
            pool.select_order("anthropic", &accounts, Some(session), None, Some(&pool_cfg));
        let sticky = initial[0];
        let other = initial[1];
        stamp_near_quota(&pool, "anthropic", &accounts[sticky], unix_now() - 10_000);

        let order = pool.select_order("anthropic", &accounts, Some(session), None, Some(&pool_cfg));
        assert_eq!(
            order,
            vec![other, sticky],
            "a stale near-quota Claude account is never promoted, no matter how stale"
        );
    }

    #[test]
    fn probe_candidates_come_from_rotation_representatives() {
        let pool_cfg = PoolConfig {
            default_threshold: Some(0.5),
            reprobe_seconds: Some(60),
            ..Default::default()
        };
        let pool = Arc::new(AccountPool::new());
        let mut alias1 = account_with_uuid("codex-alias-1", "shared-uuid");
        alias1.store_family = Some(StoreFamily::Chatgpt);
        let mut alias2 = account_with_uuid("codex-alias-2", "shared-uuid");
        alias2.store_family = Some(StoreFamily::Chatgpt);
        let mut other = account("codex-other");
        other.store_family = Some(StoreFamily::Chatgpt);
        let accounts = vec![alias1, alias2, other];
        let session = "shared-identity-probe";

        // Both aliases resolve to the same `AccountKey` (identical uuid and
        // store_family), so this single stamp covers whichever alias
        // `collapse_representatives` picks as the representative too.
        stamp_near_quota(&pool, "codex", &accounts[0], unix_now() - 61);

        let (order, reservation) =
            pool.select_order_deferred("codex", &accounts, Some(session), None, Some(&pool_cfg));
        assert_eq!(
            order.len(),
            2,
            "the two aliases collapse to one rotation slot, plus the other account"
        );
        assert!(
            !order.contains(&1),
            "the non-representative alias never enters the rotation at all"
        );
        assert_eq!(
            order[0], 0,
            "the promoted index is the rotation representative (index 0 wins equal priority/disabled ties)"
        );
        assert!(reservation.is_some());
        let mut seen = HashSet::new();
        for &index in &order {
            assert!(seen.insert(index), "no index appears twice in the order");
        }
    }

    #[test]
    fn cooling_or_disabled_account_never_probes() {
        let pool_cfg = PoolConfig {
            default_threshold: Some(0.5),
            reprobe_seconds: Some(60),
            ..Default::default()
        };

        let pool = AccountPool::new();
        let mut a = account("codex-a");
        a.store_family = Some(StoreFamily::Chatgpt);
        let mut b = account("codex-b");
        b.store_family = Some(StoreFamily::Chatgpt);
        let accounts = vec![a, b];
        let session = "cooling-never-probes";
        let initial = pool.select_order("codex", &accounts, Some(session), None, Some(&pool_cfg));
        let sticky = initial[0];
        stamp_near_quota(&pool, "codex", &accounts[sticky], unix_now() - 61);
        pool.cooldown("codex", &accounts[sticky], Duration::from_secs(300), "test");

        let order = pool.select_order("codex", &accounts, Some(session), None, Some(&pool_cfg));
        assert_ne!(
            order[0], sticky,
            "a cooling-down account is never probed even when stale and near"
        );

        // A disabled account never enters `rotation` at all, so it is
        // structurally unreachable as a probe candidate -- checked directly
        // with an isolated pool.
        let pool2 = AccountPool::new();
        let mut disabled = account("codex-disabled");
        disabled.store_family = Some(StoreFamily::Chatgpt);
        disabled.disabled = true;
        let mut enabled = account("codex-enabled");
        enabled.store_family = Some(StoreFamily::Chatgpt);
        let disabled_accounts = vec![disabled, enabled];
        stamp_near_quota(&pool2, "codex", &disabled_accounts[0], unix_now() - 61);

        let order2 = pool2.select_order(
            "codex",
            &disabled_accounts,
            Some(session),
            None,
            Some(&pool_cfg),
        );
        assert!(
            !order2.contains(&0),
            "a disabled account is never a probe candidate"
        );
    }

    #[test]
    fn reprobe_zero_disables_probing() {
        let pool_cfg = PoolConfig {
            default_threshold: Some(0.5),
            reprobe_seconds: Some(0),
            ..Default::default()
        };
        let pool = AccountPool::new();
        let mut a = account("codex-a");
        a.store_family = Some(StoreFamily::Chatgpt);
        let mut b = account("codex-b");
        b.store_family = Some(StoreFamily::Chatgpt);
        let accounts = vec![a, b];
        let session = "reprobe-zero";
        let initial = pool.select_order("codex", &accounts, Some(session), None, Some(&pool_cfg));
        let sticky = initial[0];
        let other = initial[1];
        // Stale well past any reasonable reprobe interval, but still inside
        // `WINDOW_5H_SECS` so `expire_stale_quota` does not wipe the mark
        // itself before `assess_quota` ever sees it.
        stamp_near_quota(&pool, "codex", &accounts[sticky], unix_now() - 3_600);

        let order = pool.select_order("codex", &accounts, Some(session), None, Some(&pool_cfg));
        assert_eq!(
            order,
            vec![other, sticky],
            "reprobe_seconds = 0 disables probing entirely, however stale"
        );
    }

    #[test]
    fn absent_pool_config_disables_probing() {
        let pool = AccountPool::new();
        let mut a = account("codex-a");
        a.store_family = Some(StoreFamily::Chatgpt);
        let mut b = account("codex-b");
        b.store_family = Some(StoreFamily::Chatgpt);
        let accounts = vec![a, b];
        let session = "no-pool-config";
        let initial = pool.select_order("codex", &accounts, Some(session), None, None);
        let sticky = initial[0];
        let other = initial[1];
        // Without `[server.pool]`, the legacy 0.98 hard threshold alone still
        // marks very high utilization "near" (the pre-#135 contract), but
        // that must never translate into a probe promotion. Stale well past
        // any reasonable reprobe interval, but still inside `WINDOW_5H_SECS`
        // so `expire_stale_quota` does not wipe the mark itself first.
        {
            let mut entries = pool.entries.lock().expect("account health lock poisoned");
            let health = entries
                .entry(account_key("codex", &accounts[sticky]))
                .or_default();
            health.quota.utilization_5h = Some(0.99);
            health.quota.observed_at_5h = Some(unix_now() - 3_600);
        }

        let order = pool.select_order("codex", &accounts, Some(session), None, None);
        assert_eq!(
            order,
            vec![other, sticky],
            "an absent [server.pool] disables probing regardless of staleness (pre-#135 behavior)"
        );
    }

    #[test]
    fn probe_promotes_over_healthy_sticky_fast_path() {
        let pool_cfg = PoolConfig {
            default_threshold: Some(0.5),
            reprobe_seconds: Some(60),
            ..Default::default()
        };
        let pool = Arc::new(AccountPool::new());
        let mut a = account("codex-a");
        a.store_family = Some(StoreFamily::Chatgpt);
        let mut b = account("codex-b");
        b.store_family = Some(StoreFamily::Chatgpt);
        let accounts = vec![a, b];
        let session = "fast-path-promotion";
        let initial = pool.select_order("codex", &accounts, Some(session), None, Some(&pool_cfg));
        let sticky = initial[0];
        let other = initial[1];
        // The non-sticky account is stale and near; sticky itself stays
        // healthy, so without Change B this takes the sticky fast path and
        // returns `rotation` (sticky first) completely untouched.
        stamp_near_quota(&pool, "codex", &accounts[other], unix_now() - 61);

        let (order, reservation) =
            pool.select_order_deferred("codex", &accounts, Some(session), None, Some(&pool_cfg));
        assert_eq!(
            order,
            vec![other, sticky],
            "the stale near candidate is promoted even over a healthy sticky account"
        );
        assert!(reservation.is_some());
    }
}
