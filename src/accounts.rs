use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use reqwest::{header::HeaderMap, StatusCode};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;

use crate::config::AccountConfig;

type AccountKey = (String, String);
type RefreshLock = Arc<AsyncMutex<()>>;

const SWITCH_THRESHOLD: f64 = 0.98;

#[derive(Debug, Default)]
pub struct QuotaState {
    pub utilization_5h: Option<f64>,
    pub reset_5h: Option<u64>,
    pub utilization_7d: Option<f64>,
    pub reset_7d: Option<u64>,
    pub utilization_7d_oi: Option<f64>,
    pub reset_7d_oi: Option<u64>,
    pub status: Option<String>,
}

#[derive(Debug, Default)]
struct AccountHealth {
    cooldown_until: Option<Instant>,
    quota: QuotaState,
    /// Whether the pool has processed at least one upstream response for this
    /// account (a quota update, a cooldown, or a healthy-mark). `select_order`
    /// inserts a default entry on selection, so entry existence alone does not
    /// mean an account has been observed — the admin dashboard's `has_state`
    /// keys off this flag instead of mere entry presence.
    observed: bool,
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
    /// Derived: not cooling down and not at or above the switch threshold.
    pub available: bool,
    pub near_quota: bool,
    /// Seconds until the current cooldown expires, when the account is cooling.
    pub cooldown_secs_remaining: Option<u64>,
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
    fn unseen(name: &str) -> Self {
        Self {
            name: name.to_string(),
            has_state: false,
            available: true,
            near_quota: false,
            cooldown_secs_remaining: None,
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
}

impl AccountPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return account indices in the order an adapter should try them.
    pub fn select_order(
        &self,
        provider: &str,
        accounts: &[AccountConfig],
        session_id: Option<&str>,
        model: Option<&str>,
    ) -> Vec<usize> {
        if accounts.is_empty() {
            return Vec::new();
        }

        let start = match session_id {
            Some(session_id) => stable_session_index(session_id, accounts.len()),
            None => {
                let mut counters = self.rr.lock().expect("account round-robin lock poisoned");
                let counter = counters.entry(provider.to_string()).or_default();
                let start = *counter % accounts.len();
                *counter = counter.wrapping_add(1);
                start
            }
        };

        let now = Instant::now();
        let unix_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let snapshots = {
            let mut entries = self.entries.lock().expect("account health lock poisoned");
            accounts
                .iter()
                .map(|account| {
                    let health = entries
                        .entry((provider.to_string(), account.name.clone()))
                        .or_default();
                    expire_stale_quota(&mut health.quota, unix_now);
                    (
                        health.cooldown_until,
                        near_quota(health, model, SWITCH_THRESHOLD),
                        governing_weekly_reset(health, model),
                    )
                })
                .collect::<Vec<_>>()
        };

        let rotation = (0..accounts.len())
            .map(|offset| (start + offset) % accounts.len())
            .collect::<Vec<_>>();
        let (sticky_cooldown, sticky_near_quota, _) = snapshots[start];
        if sticky_cooldown.is_none_or(|until| until <= now) && !sticky_near_quota {
            return rotation;
        }

        let mut available_under = rotation
            .iter()
            .copied()
            .filter(|&index| {
                snapshots[index].0.is_none_or(|until| until <= now) && !snapshots[index].1
            })
            .collect::<Vec<_>>();
        // `Option` orders `None` before `Some`, and the stable sort preserves
        // rotation order as the tiebreak for equal reset timestamps.
        available_under.sort_by_key(|&index| snapshots[index].2);

        let available_over = rotation.iter().copied().filter(|&index| {
            snapshots[index].0.is_none_or(|until| until <= now) && snapshots[index].1
        });

        let mut cooled = rotation
            .iter()
            .copied()
            .filter(|&index| snapshots[index].0.is_some_and(|until| until > now))
            .collect::<Vec<_>>();
        cooled.sort_by_key(|&index| snapshots[index].0);

        available_under
            .into_iter()
            .chain(available_over)
            .chain(cooled)
            .collect()
    }

    pub fn note_quota(&self, provider: &str, account: &str, headers: &HeaderMap) {
        let mut entries = self.entries.lock().expect("account health lock poisoned");
        let health = entries
            .entry((provider.to_string(), account.to_string()))
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
    }

    pub fn cooldown(&self, provider: &str, account: &str, duration: Duration) {
        let mut entries = self.entries.lock().expect("account health lock poisoned");
        let health = entries
            .entry((provider.to_string(), account.to_string()))
            .or_default();
        health.observed = true;
        health.cooldown_until = Some(Instant::now() + duration);
    }

    pub fn mark_healthy(&self, provider: &str, account: &str) {
        let mut entries = self.entries.lock().expect("account health lock poisoned");
        let health = entries
            .entry((provider.to_string(), account.to_string()))
            .or_default();
        health.observed = true;
        health.cooldown_until = None;
    }

    /// Drop all pool health for `account` across every provider. The admin
    /// surface calls this when it re-provisions or removes an account so
    /// cooldown/quota accumulated under a prior token does not carry onto the
    /// replacement (pool state is process-lifetime and keyed only by name).
    pub fn forget(&self, account: &str) {
        let mut entries = self.entries.lock().expect("account health lock poisoned");
        entries.retain(|(_, name), _| name != account);
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
    ) -> Vec<AccountSnapshot> {
        let now = Instant::now();
        let unix_now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut entries = self.entries.lock().expect("account health lock poisoned");
        accounts
            .iter()
            .map(|account| {
                let key = (provider.to_string(), account.name.clone());
                let Some(health) = entries.get_mut(&key).filter(|health| health.observed) else {
                    // Never selected, or selected but not yet answered (a default
                    // entry from `select_order`): report a clean, available slot.
                    return AccountSnapshot::unseen(&account.name);
                };
                expire_stale_quota(&mut health.quota, unix_now);
                let near = near_quota(health, model, SWITCH_THRESHOLD);
                let cooldown_secs_remaining = health
                    .cooldown_until
                    .and_then(|until| until.checked_duration_since(now))
                    .map(|remaining| remaining.as_secs());
                let cooling = cooldown_secs_remaining.is_some();
                AccountSnapshot {
                    name: account.name.clone(),
                    has_state: true,
                    available: !cooling && !near,
                    near_quota: near,
                    cooldown_secs_remaining,
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

    /// Get the async mutex that serializes token refreshes for one account.
    ///
    /// The map's synchronous mutex is released before the returned lock can be
    /// awaited by the caller.
    pub fn refresh_lock(&self, provider: &str, account: &str) -> Arc<AsyncMutex<()>> {
        let mut locks = self
            .refresh_locks
            .lock()
            .expect("account refresh-lock map poisoned");
        Arc::clone(
            locks
                .entry((provider.to_string(), account.to_string()))
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        )
    }
}

fn stable_session_index(session_id: &str, account_count: usize) -> usize {
    let digest = Sha256::digest(session_id.as_bytes());
    let prefix = u64::from_be_bytes(digest[..8].try_into().expect("SHA-256 prefix is 8 bytes"));
    (prefix % account_count as u64) as usize
}

fn update_header<T: std::str::FromStr>(headers: &HeaderMap, name: &str, field: &mut Option<T>) {
    if let Some(parsed) = headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<T>().ok())
    {
        *field = Some(parsed);
    }
}

fn is_fable_model(model: Option<&str>) -> bool {
    model.is_some_and(|model| model.to_ascii_lowercase().contains("fable"))
}

fn governing_weekly_utilization(health: &AccountHealth, model: Option<&str>) -> Option<f64> {
    if is_fable_model(model) {
        health
            .quota
            .utilization_7d_oi
            .or(health.quota.utilization_7d)
    } else {
        health.quota.utilization_7d
    }
}

fn governing_weekly_reset(health: &AccountHealth, model: Option<&str>) -> Option<u64> {
    if is_fable_model(model) && health.quota.utilization_7d_oi.is_some() {
        health.quota.reset_7d_oi
    } else {
        health.quota.reset_7d
    }
}

fn near_quota(health: &AccountHealth, model: Option<&str>, threshold: f64) -> bool {
    health.quota.status.as_deref() == Some("rejected")
        || health
            .quota
            .utilization_5h
            .is_some_and(|utilization| utilization >= threshold)
        || governing_weekly_utilization(health, model)
            .is_some_and(|utilization| utilization >= threshold)
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
/// share one call site, but Codex responses carry no per-account
/// quota-rejection header — unlike Anthropic, every 429 rotates rather than
/// pausing the same account, so `headers` goes unused for now.
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
            credentials: None,
            token_env: None,
            uuid: None,
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
    fn session_selection_is_stable_and_spreads_across_sessions() {
        let pool = AccountPool::new();
        let accounts = accounts();
        let first = pool.select_order("anthropic", &accounts, Some("session-a"), None);
        assert_eq!(
            first,
            pool.select_order("anthropic", &accounts, Some("session-a"), None)
        );
        assert_eq!(first[0], stable_session_index("session-a", accounts.len()));

        let starts = (0..64)
            .map(|id| {
                pool.select_order("anthropic", &accounts, Some(&format!("session-{id}")), None)[0]
            })
            .collect::<HashSet<_>>();
        assert!(starts.len() > 1);
    }

    #[test]
    fn healthy_under_threshold_sticky_account_stays_first() {
        let pool = AccountPool::new();
        let accounts = accounts();
        let session = "healthy-sticky";
        let first = pool.select_order("anthropic", &accounts, Some(session), None);
        let sticky = &accounts[first[0]].name;
        pool.note_quota(
            "anthropic",
            sticky,
            &quota_headers(&[(
                "anthropic-ratelimit-unified-5h-utilization",
                "0.97".to_string(),
            )]),
        );
        assert_eq!(
            pool.select_order("anthropic", &accounts, Some(session), None),
            first
        );
    }

    #[test]
    fn near_quota_sticky_rotates_to_fresh_account() {
        let pool = AccountPool::new();
        let accounts = accounts();
        let session = "quota-sticky";
        let original = pool.select_order("anthropic", &accounts, Some(session), None);
        let sticky = original[0];
        pool.note_quota(
            "anthropic",
            &accounts[sticky].name,
            &quota_headers(&[(
                "anthropic-ratelimit-unified-5h-utilization",
                "0.98".to_string(),
            )]),
        );
        let rotated = pool.select_order("anthropic", &accounts, Some(session), None);
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
            "seen-near",
            &quota_headers(&[(
                "anthropic-ratelimit-unified-5h-utilization",
                "0.99".to_string(),
            )]),
        );
        pool.cooldown("anthropic", "seen-cool", Duration::from_secs(45));

        let snaps = pool.snapshot("anthropic", &accounts, None);
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
    fn under_quota_accounts_sort_by_weekly_reset_with_unknown_first() {
        let pool = AccountPool::new();
        let accounts = vec![account("a"), account("b"), account("c"), account("d")];
        let session = "reset-sort";
        let rotation = pool.select_order("anthropic", &accounts, Some(session), None);
        let sticky = rotation[0];
        pool.note_quota(
            "anthropic",
            &accounts[sticky].name,
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
                    &accounts[index].name,
                    &quota_headers(&[("anthropic-ratelimit-unified-7d-reset", reset.to_string())]),
                );
            }
        }
        let selected = pool.select_order("anthropic", &accounts, Some(session), None);
        assert_eq!(selected[..3], [rotation[1], rotation[2], rotation[3]]);
        assert_eq!(selected[3], sticky);
    }

    #[test]
    fn fable_uses_oi_bucket_while_other_models_use_shared_weekly_bucket() {
        let pool = AccountPool::new();
        let accounts = accounts();
        let session = "model-aware";
        let rotation = pool.select_order("anthropic", &accounts, Some(session), None);
        let sticky = rotation[0];
        pool.note_quota(
            "anthropic",
            &accounts[sticky].name,
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
            )[0],
            sticky
        );
        assert_ne!(
            pool.select_order(
                "anthropic",
                &accounts,
                Some(session),
                Some("CLAUDE-FABLE-5"),
            )[0],
            sticky
        );
    }

    #[test]
    fn note_quota_parses_preserves_and_expires_fields() {
        let pool = AccountPool::new();
        let accounts = vec![account("a"), account("b")];
        let session = "expiry";
        let rotation = pool.select_order("anthropic", &accounts, Some(session), None);
        let sticky = rotation[0];
        let past = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(1);
        pool.note_quota(
            "anthropic",
            &accounts[sticky].name,
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

        let selected = pool.select_order("anthropic", &accounts, Some(session), None);
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
    fn cooldown_skips_accounts_and_all_cooled_uses_soonest_expiry() {
        let pool = AccountPool::new();
        let accounts = vec![account("a"), account("b"), account("c")];
        let sticky = pool.select_order("anthropic", &accounts, Some("sticky"), None)[0];
        pool.cooldown("anthropic", &accounts[sticky].name, Duration::from_secs(30));
        let available = pool.select_order("anthropic", &accounts, Some("sticky"), None);
        assert_eq!(available.len(), 3);
        assert_eq!(available[2], sticky);

        for (index, seconds) in [(0, 30), (1, 20), (2, 10)] {
            pool.cooldown(
                "anthropic",
                &accounts[index].name,
                Duration::from_secs(seconds),
            );
        }
        assert_eq!(
            pool.select_order("anthropic", &accounts, Some("sticky"), None),
            vec![2, 1, 0]
        );
    }

    #[test]
    fn round_robin_counters_are_independent_per_provider() {
        let pool = AccountPool::new();
        let accounts = accounts();
        assert_eq!(pool.select_order("one", &accounts, None, None)[0], 0);
        assert_eq!(pool.select_order("one", &accounts, None, None)[0], 1);
        assert_eq!(pool.select_order("two", &accounts, None, None)[0], 0);
        assert_eq!(pool.select_order("one", &accounts, None, None)[0], 2);
        assert_eq!(pool.select_order("two", &accounts, None, None)[0], 1);
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
        // Codex has no per-account quota-rejection header, so every 429
        // rotates — unlike Anthropic's PauseSame-without-a-rejected-header.
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
}
