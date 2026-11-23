//! Per-IP fixed-window rate limiter used by the device-flow endpoints.
//!
//! Counters are deliberately memory-only: windows are short, so a restart only
//! resets them. Idle entries are swept on every check and the table is capped
//! at [`MAX_RATE_LIMIT_IDENTITIES`] so an address sweep cannot grow it without
//! bound.

use std::{
    collections::HashMap,
    sync::Mutex,
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
        let mut limits = self
            .limits
            .lock()
            .expect("gateway rate-limit lock poisoned");
        limits.retain(|_, entry| now.saturating_duration_since(entry.last_seen) < self.window);
        if !limits.contains_key(ip) && limits.len() >= MAX_RATE_LIMIT_IDENTITIES {
            return false;
        }
        let entry = limits.entry(ip.to_string()).or_insert(RateLimitEntry {
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
        entry.count <= self.max
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
}
