//! Keyed fixed-window rate limiter used by the device-flow endpoints and the
//! token exchange (`POST /v1/oauth/token`).
//!
//! The key is caller-chosen: a client address for the device-flow endpoints
//! ([`PerIpRateLimiter::check`]) and for exchange failures
//! ([`PerIpRateLimiter::peek`] then [`PerIpRateLimiter::record_failure`]), and
//! a digest of rule fingerprint plus verified identity for successful
//! exchanges ([`PerIpRateLimiter::admit`]).
//!
//! [`PerIpRateLimiter::peek`] does not count, so requests already in flight
//! when an address reaches its limit are not refused: the limit applies once
//! their failures are recorded.
//!
//! Counters are deliberately memory-only: windows are short, so a restart only
//! resets them. Idle entries are swept on every check and the table is capped
//! at [`MAX_RATE_LIMIT_IDENTITIES`] so a key sweep cannot grow it without
//! bound.

use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

const MAX_RATE_LIMIT_IDENTITIES: usize = 4096;

struct RateLimitEntry {
    window_start: Instant,
    count: u32,
    last_seen: Instant,
}

pub struct PerIpRateLimiter {
    limits: Mutex<HashMap<String, RateLimitEntry>>,
    window: Duration,
    max: u32,
}

impl PerIpRateLimiter {
    pub fn new(window: Duration, max: u32) -> Self {
        Self {
            limits: Mutex::new(HashMap::new()),
            window,
            max,
        }
    }

    pub fn check(&self, ip: &str) -> bool {
        self.check_at(ip, Instant::now())
    }

    fn check_at(&self, ip: &str, now: Instant) -> bool {
        matches!(self.count_at(ip, now), Counted::Within)
    }

    /// Shared core of every counting entry point: sweep idle entries, refuse
    /// an untracked key when the table is full, reset an elapsed window, then
    /// count one attempt. The key is copied only when it is new, so a tracked
    /// key costs no allocation under the lock.
    fn count_at(&self, key: &str, now: Instant) -> Counted {
        let mut limits = self.lock_and_sweep(now);
        if let Some(entry) = limits.get_mut(key) {
            return self.bump(entry, now);
        }
        if limits.len() >= MAX_RATE_LIMIT_IDENTITIES {
            return Counted::TableFull;
        }
        let entry = limits.entry(key.to_owned()).or_insert(RateLimitEntry {
            window_start: now,
            count: 0,
            last_seen: now,
        });
        self.bump(entry, now)
    }

    fn bump(&self, entry: &mut RateLimitEntry, now: Instant) -> Counted {
        if now.saturating_duration_since(entry.window_start) >= self.window {
            entry.window_start = now;
            entry.count = 0;
        }
        entry.last_seen = now;
        entry.count += 1;
        if entry.count <= self.max {
            Counted::Within
        } else {
            Counted::Over(self.remaining(entry, now))
        }
    }

    /// Every key currently tracked, for tests that assert on stored key size.
    #[cfg(test)]
    pub(crate) fn tracked_keys(&self) -> Vec<String> {
        let limits = self
            .limits
            .lock()
            .expect("gateway rate-limit lock poisoned");
        limits.keys().cloned().collect()
    }

    fn lock_and_sweep(&self, now: Instant) -> MutexGuard<'_, HashMap<String, RateLimitEntry>> {
        let mut limits = self
            .limits
            .lock()
            .expect("gateway rate-limit lock poisoned");
        limits.retain(|_, entry| now.saturating_duration_since(entry.last_seen) < self.window);
        limits
    }

    fn remaining(&self, entry: &RateLimitEntry, now: Instant) -> Duration {
        self.window
            .saturating_sub(now.saturating_duration_since(entry.window_start))
    }

    /// Whole seconds left, rounded up and clamped to `[1, window]`, so a
    /// sub-second remainder still tells the caller to wait one second.
    fn retry_after(&self, remaining: Duration) -> RetryAfter {
        let window = self.window.as_secs() + u64::from(self.window.subsec_nanos() > 0);
        let left = remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0);
        RetryAfter(left.clamp(1, window.max(1)))
    }

    fn refusal(&self, counted: Counted) -> Result<(), RetryAfter> {
        match counted {
            Counted::Within => Ok(()),
            Counted::Over(remaining) => Err(self.retry_after(remaining)),
            Counted::TableFull => Err(self.retry_after(self.window)),
        }
    }
}

/// Outcome of counting one attempt.
enum Counted {
    Within,
    /// Over the limit; carries the time left in the key's window.
    Over(Duration),
    /// The key is untracked and the table is full; nothing was recorded.
    TableFull,
}

/// Whole seconds a caller should wait before retrying, for a `Retry-After`
/// header. Always at least 1 and never more than the limiter's window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryAfter(u64);

impl RetryAfter {
    pub fn seconds(self) -> u64 {
        self.0
    }
}

impl PerIpRateLimiter {
    /// Non-counting: is `key` already over its limit within the current window?
    /// An untracked key is never blocked, even when the table is full.
    pub fn peek(&self, key: &str) -> Result<(), RetryAfter> {
        self.peek_at(key, Instant::now())
    }

    fn peek_at(&self, key: &str, now: Instant) -> Result<(), RetryAfter> {
        let limits = self.lock_and_sweep(now);
        match limits.get(key) {
            Some(entry)
                if entry.count >= self.max
                    && now.saturating_duration_since(entry.window_start) < self.window =>
            {
                Err(self.retry_after(self.remaining(entry, now)))
            }
            _ => Ok(()),
        }
    }

    /// Counting: record one failed attempt for `key`.
    pub fn record_failure(&self, key: &str) -> Result<(), RetryAfter> {
        self.record_failure_at(key, Instant::now())
    }

    fn record_failure_at(&self, key: &str, now: Instant) -> Result<(), RetryAfter> {
        // Only a refused (untracked, table-full) key is an error here: the
        // failure itself is recorded, and the next request is blocked by `peek`.
        match self.count_at(key, now) {
            Counted::TableFull => self.refusal(Counted::TableFull),
            _ => Ok(()),
        }
    }

    /// Counting: admit `key` unless its count now exceeds the limit.
    pub fn admit(&self, key: &str) -> Result<(), RetryAfter> {
        self.admit_at(key, Instant::now())
    }

    fn admit_at(&self, key: &str, now: Instant) -> Result<(), RetryAfter> {
        self.refusal(self.count_at(key, now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_rejects_new_identity_at_capacity() {
        let limiter = PerIpRateLimiter::new(Duration::from_secs(60), 1);
        let now = Instant::now();
        for index in 0..MAX_RATE_LIMIT_IDENTITIES {
            assert!(limiter.check_at(&format!("client-{index}"), now));
        }
        assert!(!limiter.check_at("overflow", now));
        assert!(!limiter.check_at("client-0", now));
    }

    #[test]
    fn rate_limit_sweep_evicts_idle_ips() {
        let limiter = PerIpRateLimiter::new(Duration::from_secs(60), 1);
        let now = Instant::now();
        assert!(limiter.check_at("192.0.2.1", now));
        assert!(limiter.check_at("192.0.2.2", now));
        assert_eq!(limiter.limits.lock().unwrap().len(), 2);

        assert!(limiter.check_at("192.0.2.3", now + Duration::from_secs(60)));
        let limits = limiter.limits.lock().unwrap();
        assert_eq!(limits.len(), 1);
        assert!(limits.contains_key("192.0.2.3"));
    }

    #[test]
    fn rate_limit_is_scoped_by_ip() {
        let limiter = PerIpRateLimiter::new(Duration::from_secs(60), 1);
        assert!(limiter.check("192.0.2.1"));
        assert!(!limiter.check("192.0.2.1"));
        assert!(limiter.check("192.0.2.2"));
    }

    fn secs(result: Result<(), RetryAfter>) -> Option<u64> {
        result.err().map(RetryAfter::seconds)
    }

    fn fill(limiter: &PerIpRateLimiter, now: Instant) {
        for index in 0..MAX_RATE_LIMIT_IDENTITIES {
            assert!(limiter.check_at(&format!("client-{index}"), now));
        }
    }

    #[test]
    fn admit_blocks_over_max_until_the_window_elapses() {
        let limiter = PerIpRateLimiter::new(Duration::from_secs(60), 2);
        let now = Instant::now();
        assert!(limiter.admit_at("id", now).is_ok());
        assert!(limiter.admit_at("id", now).is_ok());
        assert_eq!(secs(limiter.admit_at("id", now)), Some(60));
        assert_eq!(
            secs(limiter.admit_at("id", now + Duration::from_secs(59))),
            Some(1)
        );
        assert!(limiter
            .admit_at("id", now + Duration::from_secs(60))
            .is_ok());
    }

    #[test]
    fn peek_does_not_count_and_blocks_only_after_max_failures() {
        let limiter = PerIpRateLimiter::new(Duration::from_secs(60), 2);
        let now = Instant::now();
        for _ in 0..10 {
            assert!(limiter.peek_at("ip", now).is_ok());
        }
        assert!(limiter.record_failure_at("ip", now).is_ok());
        assert!(limiter.peek_at("ip", now).is_ok());
        assert!(limiter.record_failure_at("ip", now).is_ok());
        assert_eq!(secs(limiter.peek_at("ip", now)), Some(60));
        assert_eq!(
            secs(limiter.peek_at("ip", now + Duration::from_secs(30))),
            Some(30)
        );
        assert!(limiter.peek_at("ip", now + Duration::from_secs(60)).is_ok());
    }

    #[test]
    fn retry_after_rounds_a_sub_second_remainder_up_to_one() {
        let limiter = PerIpRateLimiter::new(Duration::from_secs(60), 1);
        let now = Instant::now();
        assert!(limiter.record_failure_at("ip", now).is_ok());
        let late = now + Duration::from_millis(59_999);
        assert_eq!(secs(limiter.peek_at("ip", late)), Some(1));
        let rounded = now + Duration::from_millis(58_500);
        assert_eq!(secs(limiter.peek_at("ip", rounded)), Some(2));
    }

    #[test]
    fn retry_after_stays_within_one_and_the_window() {
        let limiter = PerIpRateLimiter::new(Duration::from_millis(1500), 1);
        let now = Instant::now();
        assert!(limiter.record_failure_at("ip", now).is_ok());
        for offset in [0u64, 1, 500, 1000, 1499] {
            let seconds = secs(limiter.peek_at("ip", now + Duration::from_millis(offset)))
                .expect("blocked inside the window");
            assert!((1..=2).contains(&seconds), "{offset}ms -> {seconds}");
        }
    }

    #[test]
    fn a_full_table_refuses_a_new_identity_but_not_a_tracked_one() {
        let limiter = PerIpRateLimiter::new(Duration::from_secs(60), 5);
        let now = Instant::now();
        fill(&limiter, now);
        assert_eq!(secs(limiter.admit_at("new-identity", now)), Some(60));
        assert!(limiter.admit_at("client-0", now).is_ok());
    }

    #[test]
    fn a_full_table_neither_blocks_nor_tracks_an_untracked_address() {
        let limiter = PerIpRateLimiter::new(Duration::from_secs(60), 5);
        let now = Instant::now();
        fill(&limiter, now);
        assert!(limiter.peek_at("untracked", now).is_ok());
        assert_eq!(secs(limiter.record_failure_at("untracked", now)), Some(60));
        assert!(!limiter.limits.lock().unwrap().contains_key("untracked"));
        assert!(limiter.peek_at("untracked", now).is_ok());
    }

    #[test]
    fn peek_leaves_check_behaviour_unchanged() {
        let limiter = PerIpRateLimiter::new(Duration::from_secs(60), 1);
        let now = Instant::now();
        assert!(limiter.peek_at("ip", now).is_ok());
        assert!(limiter.check_at("ip", now));
        assert!(!limiter.check_at("ip", now));
        assert_eq!(secs(limiter.peek_at("ip", now)), Some(60));
    }
}
