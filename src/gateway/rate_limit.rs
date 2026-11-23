//! Keyed fixed-window rate limiter used by the device-flow endpoints and the
//! token exchange (`POST /v1/oauth/token`).
//!
//! The key is caller-chosen: a client address for the device-flow endpoints
//! ([`PerIpRateLimiter::check`]) and for exchange failures
//! ([`PerIpRateLimiter::charge`] before any work, then
//! [`PerIpRateLimiter::refund`] when the exchange did not fail), and
//! a digest of rule fingerprint plus verified identity for successful
//! exchanges ([`PerIpRateLimiter::admit`]).
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
    /// count one attempt.
    fn count_at(&self, key: &str, now: Instant) -> Counted {
        let mut limits = self.lock_and_sweep(now);
        if !limits.contains_key(key) && limits.len() >= MAX_RATE_LIMIT_IDENTITIES {
            return Counted::TableFull;
        }
        let entry = limits.entry(key.to_string()).or_insert(RateLimitEntry {
            window_start: now,
            count: 0,
            last_seen: now,
        });
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

/// One attempt counted by [`PerIpRateLimiter::charge`], to hand back to
/// [`PerIpRateLimiter::refund`] when the attempt turns out not to count.
#[derive(Debug)]
pub struct Charge(Slot);

#[derive(Debug)]
enum Slot {
    /// Counted against `key` in the window that started at `window_start`.
    Tracked { key: String, window_start: Instant },
    /// The table was full, so nothing was recorded; carries the wait to
    /// answer if the attempt fails.
    Untracked(RetryAfter),
}

impl Charge {
    /// For a charge the full table could not record, the wait to answer if
    /// the attempt fails; `None` when the attempt was counted.
    pub fn untracked(&self) -> Option<RetryAfter> {
        match self.0 {
            Slot::Tracked { .. } => None,
            Slot::Untracked(retry_after) => Some(retry_after),
        }
    }
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
    /// Charge one attempt to `key` up front, in one step with the check, so
    /// concurrent attempts cannot all pass before any of them is counted.
    ///
    /// Refused when `key` is tracked, its window is live and its count is
    /// already at the limit. An elapsed window is reset as [`Self::admit`]
    /// does. An untracked key is never refused, even when the table is full:
    /// it is then left untracked and the returned [`Charge`] says so.
    pub fn charge(&self, key: &str) -> Result<Charge, RetryAfter> {
        self.charge_at(key, Instant::now())
    }

    fn charge_at(&self, key: &str, now: Instant) -> Result<Charge, RetryAfter> {
        let mut limits = self.lock_and_sweep(now);
        if let Some(entry) = limits.get_mut(key) {
            if now.saturating_duration_since(entry.window_start) >= self.window {
                entry.window_start = now;
                entry.count = 0;
            } else if entry.count >= self.max {
                return Err(self.retry_after(self.remaining(entry, now)));
            }
            entry.last_seen = now;
            entry.count += 1;
            return Ok(Charge(Slot::Tracked {
                key: key.to_string(),
                window_start: entry.window_start,
            }));
        }
        if limits.len() >= MAX_RATE_LIMIT_IDENTITIES {
            return Ok(Charge(Slot::Untracked(self.retry_after(self.window))));
        }
        limits.insert(
            key.to_string(),
            RateLimitEntry {
                window_start: now,
                count: 1,
                last_seen: now,
            },
        );
        Ok(Charge(Slot::Tracked {
            key: key.to_string(),
            window_start: now,
        }))
    }

    /// Undo exactly the attempt `charge` counted. Nothing happens when the
    /// charge was untracked, or when its window has since rolled over or been
    /// swept, so a late refund never lowers a newer window's count.
    pub fn refund(&self, charge: Charge) {
        let Slot::Tracked { key, window_start } = charge.0 else {
            return;
        };
        let mut limits = self
            .limits
            .lock()
            .expect("gateway rate-limit lock poisoned");
        if let Some(entry) = limits.get_mut(&key) {
            if entry.window_start == window_start {
                entry.count = entry.count.saturating_sub(1);
            }
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

    fn charged(result: Result<Charge, RetryAfter>) -> Option<u64> {
        result.err().map(RetryAfter::seconds)
    }

    #[test]
    fn charge_admits_up_to_max_then_refuses_without_counting() {
        let limiter = PerIpRateLimiter::new(Duration::from_secs(60), 2);
        let now = Instant::now();
        assert!(limiter.charge_at("ip", now).is_ok());
        assert!(limiter.charge_at("ip", now).is_ok());
        for _ in 0..10 {
            assert_eq!(charged(limiter.charge_at("ip", now)), Some(60));
        }
        assert_eq!(limiter.limits.lock().unwrap()["ip"].count, 2);
        assert_eq!(
            charged(limiter.charge_at("ip", now + Duration::from_secs(30))),
            Some(30)
        );
        assert!(limiter
            .charge_at("ip", now + Duration::from_secs(60))
            .is_ok());
    }

    #[test]
    fn refund_restores_exactly_one_slot() {
        let limiter = PerIpRateLimiter::new(Duration::from_secs(60), 2);
        let now = Instant::now();
        let first = limiter.charge_at("ip", now).unwrap();
        assert!(limiter.charge_at("ip", now).is_ok());
        assert!(limiter.charge_at("ip", now).is_err());
        limiter.refund(first);
        assert_eq!(limiter.limits.lock().unwrap()["ip"].count, 1);
        assert!(limiter.charge_at("ip", now).is_ok());
        assert!(limiter.charge_at("ip", now).is_err());
    }

    #[test]
    fn refund_after_the_window_rolled_over_leaves_the_new_window_alone() {
        let limiter = PerIpRateLimiter::new(Duration::from_secs(60), 2);
        let now = Instant::now();
        let old = limiter.charge_at("ip", now).unwrap();
        let later = now + Duration::from_secs(61);
        assert!(limiter.charge_at("ip", later).is_ok());
        limiter.refund(old);
        assert_eq!(limiter.limits.lock().unwrap()["ip"].count, 1);
    }

    #[test]
    fn refund_of_a_swept_key_is_a_no_op() {
        let limiter = PerIpRateLimiter::new(Duration::from_secs(60), 2);
        let now = Instant::now();
        let old = limiter.charge_at("ip", now).unwrap();
        assert!(limiter
            .charge_at("other", now + Duration::from_secs(120))
            .is_ok());
        limiter.refund(old);
        assert!(!limiter.limits.lock().unwrap().contains_key("ip"));
    }

    #[test]
    fn retry_after_rounds_a_sub_second_remainder_up_to_one() {
        let limiter = PerIpRateLimiter::new(Duration::from_secs(60), 1);
        let now = Instant::now();
        assert!(limiter.charge_at("ip", now).is_ok());
        let late = now + Duration::from_millis(59_999);
        assert_eq!(charged(limiter.charge_at("ip", late)), Some(1));
        let rounded = now + Duration::from_millis(58_500);
        assert_eq!(charged(limiter.charge_at("ip", rounded)), Some(2));
    }

    #[test]
    fn retry_after_stays_within_one_and_the_window() {
        let limiter = PerIpRateLimiter::new(Duration::from_millis(1500), 1);
        let now = Instant::now();
        assert!(limiter.charge_at("ip", now).is_ok());
        for offset in [0u64, 1, 500, 1000, 1499] {
            let seconds = charged(limiter.charge_at("ip", now + Duration::from_millis(offset)))
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
        for _ in 0..10 {
            let charge = limiter.charge_at("untracked", now).expect("never refused");
            assert_eq!(charge.untracked().map(RetryAfter::seconds), Some(60));
            limiter.refund(charge);
        }
        assert!(!limiter.limits.lock().unwrap().contains_key("untracked"));
        assert_eq!(
            limiter.limits.lock().unwrap().len(),
            MAX_RATE_LIMIT_IDENTITIES
        );
        let tracked = limiter.charge_at("client-0", now).unwrap();
        assert!(tracked.untracked().is_none());
    }

    #[test]
    fn charging_leaves_check_behaviour_unchanged() {
        let limiter = PerIpRateLimiter::new(Duration::from_secs(60), 1);
        let now = Instant::now();
        assert!(limiter.check_at("ip", now));
        assert!(!limiter.check_at("ip", now));
        assert!(limiter.check_at("other", now));
        assert!(!limiter.check_at("other", now));
        assert_eq!(charged(limiter.charge_at("ip", now)), Some(60));
    }
}
