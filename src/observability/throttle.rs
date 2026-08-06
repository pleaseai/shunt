//! Fixed-window limiter for the Sentry events [`crate::observability`] emits
//! from the streaming path (#310).
//!
//! A mid-stream failure is reported per *request*, so a client that retries a
//! cut stream turns one underlying fault into many identical Sentry events.
//! This bounds that: per key, the first [`MAX_PER_WINDOW`] events in each
//! [`WINDOW`] are emitted and the rest are counted and dropped, with the
//! dropped count carried on the next event that is allowed through so the
//! suppression is never silent.
//!
//! Only the Sentry *event* is limited. The `shunt.stream_outcome` metric and
//! the request span's `otel.status_code` are recorded unconditionally by their
//! own call sites, so the aggregate signal stays exact regardless of what this
//! drops.

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock, PoisonError},
    time::{Duration, Instant},
};

/// Length of one fixed window.
pub(super) const WINDOW: Duration = Duration::from_secs(60);

/// Events admitted per key per [`WINDOW`]; every further event in the same
/// window is suppressed and counted.
const MAX_PER_WINDOW: u32 = 5;

/// Upper bound on distinct keys tracked at once. Keys embed the sanitized
/// model tag, which is client-supplied and therefore unbounded in cardinality
/// (#296) — beyond this, further keys are folded into [`OVERFLOW_KEY`] so the
/// table cannot grow with attacker-chosen values. At most `MAX_KEYS` distinct
/// keys plus the overflow bucket are ever held.
const MAX_KEYS: usize = 256;

/// Shared window every key beyond [`MAX_KEYS`] is folded into.
const OVERFLOW_KEY: &str = "other";

/// How long an idle key is kept before it is reclaimed, as a multiple of
/// [`WINDOW`]. A key that still owes a suppressed count is exempt: that count
/// is the payload of the next event it emits, so reclaiming it would lose the
/// only record that anything was suppressed.
const IDLE_WINDOWS: u32 = 2;

/// Whether one event may be sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ThrottleDecision {
    /// Send it. `suppressed` is how many events for the same key were dropped
    /// since the last one that was sent — zero unless a window just rolled
    /// over after hitting the cap.
    Emit { suppressed: u64 },
    /// Drop it; the cap for this key's current window is already spent.
    Suppress,
}

/// One key's current window.
struct Window {
    started_at: Instant,
    emitted: u32,
    /// Events dropped in this window, waiting to be reported on the first
    /// event admitted after it rolls over.
    suppressed: u64,
}

/// A fixed-window limiter keyed by opaque strings. Holds no clock of its own —
/// `now` is a parameter — so window behavior is testable without sleeping.
pub(super) struct EventThrottle {
    window: Duration,
    max_per_window: u32,
    max_keys: usize,
    windows: HashMap<String, Window>,
}

impl EventThrottle {
    pub(super) fn new(window: Duration, max_per_window: u32, max_keys: usize) -> Self {
        Self {
            window,
            max_per_window,
            max_keys,
            windows: HashMap::new(),
        }
    }

    /// Decide whether the event identified by `key` may be sent at `now`.
    pub(super) fn admit(&mut self, key: &str, now: Instant) -> ThrottleDecision {
        self.reclaim_idle(now);
        let key = if self.windows.contains_key(key) || self.windows.len() < self.max_keys {
            key
        } else {
            OVERFLOW_KEY
        };

        match self.windows.get_mut(key) {
            Some(window) if now.duration_since(window.started_at) < self.window => {
                if window.emitted < self.max_per_window {
                    window.emitted += 1;
                    ThrottleDecision::Emit { suppressed: 0 }
                } else {
                    window.suppressed = window.suppressed.saturating_add(1);
                    ThrottleDecision::Suppress
                }
            }
            Some(window) => {
                // The window rolled over: this event opens a new one and
                // carries whatever the old one dropped.
                let suppressed = std::mem::take(&mut window.suppressed);
                window.started_at = now;
                window.emitted = 1;
                ThrottleDecision::Emit { suppressed }
            }
            None => {
                self.windows.insert(
                    key.to_owned(),
                    Window {
                        started_at: now,
                        emitted: 1,
                        suppressed: 0,
                    },
                );
                ThrottleDecision::Emit { suppressed: 0 }
            }
        }
    }

    fn reclaim_idle(&mut self, now: Instant) {
        let idle_after = self.window * IDLE_WINDOWS;
        self.windows.retain(|_, window| {
            window.suppressed > 0 || now.duration_since(window.started_at) < idle_after
        });
    }
}

/// Decide whether one Sentry stream-failure event may be sent, against the
/// process-global window table — or, under `cfg(test)`, against a thread-local
/// one installed by [`test_support::scoped`] so tests running in parallel in
/// one process do not consume each other's budget.
pub(super) fn admit(key: &str, now: Instant) -> ThrottleDecision {
    #[cfg(test)]
    if let Some(decision) = test_support::admit(key, now) {
        return decision;
    }
    global()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .admit(key, now)
}

fn global() -> &'static Mutex<EventThrottle> {
    static THROTTLE: OnceLock<Mutex<EventThrottle>> = OnceLock::new();
    THROTTLE.get_or_init(|| Mutex::new(EventThrottle::new(WINDOW, MAX_PER_WINDOW, MAX_KEYS)))
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::{cell::RefCell, time::Instant};

    use super::{EventThrottle, ThrottleDecision, MAX_KEYS, MAX_PER_WINDOW, WINDOW};

    thread_local! {
        static OVERRIDE: RefCell<Option<EventThrottle>> = const { RefCell::new(None) };
    }

    /// Restores the process-global throttle for this thread when dropped.
    pub(crate) struct ScopedThrottle(());

    impl Drop for ScopedThrottle {
        fn drop(&mut self) {
            OVERRIDE.with(|slot| *slot.borrow_mut() = None);
        }
    }

    /// Install a fresh, empty throttle for the current thread for as long as
    /// the returned guard lives. Any test that drives a Sentry-emitting path
    /// needs this, or it shares the process-global windows with every other
    /// test and its event count stops being a property of the test itself.
    #[must_use]
    pub(crate) fn scoped() -> ScopedThrottle {
        OVERRIDE.with(|slot| {
            *slot.borrow_mut() = Some(EventThrottle::new(WINDOW, MAX_PER_WINDOW, MAX_KEYS));
        });
        ScopedThrottle(())
    }

    pub(super) fn admit(key: &str, now: Instant) -> Option<ThrottleDecision> {
        OVERRIDE.with(|slot| {
            slot.borrow_mut()
                .as_mut()
                .map(|throttle| throttle.admit(key, now))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, Instant},
    };

    use super::{EventThrottle, ThrottleDecision, MAX_KEYS, MAX_PER_WINDOW, OVERFLOW_KEY, WINDOW};

    fn throttle() -> EventThrottle {
        EventThrottle::new(WINDOW, MAX_PER_WINDOW, MAX_KEYS)
    }

    #[test]
    fn admits_up_to_the_cap_then_suppresses_within_one_window() {
        let mut throttle = throttle();
        let now = Instant::now();

        for _ in 0..MAX_PER_WINDOW {
            assert_eq!(
                throttle.admit("key", now),
                ThrottleDecision::Emit { suppressed: 0 }
            );
        }
        for _ in 0..3 {
            assert_eq!(throttle.admit("key", now), ThrottleDecision::Suppress);
        }
    }

    #[test]
    fn reports_the_suppressed_count_on_the_first_event_of_the_next_window() {
        let mut throttle = throttle();
        let start = Instant::now();

        for _ in 0..MAX_PER_WINDOW + 7 {
            throttle.admit("key", start);
        }

        // The next window opens: the first event through carries exactly the
        // seven that were dropped, and the one after it starts clean again.
        let next_window = start + WINDOW;
        assert_eq!(
            throttle.admit("key", next_window),
            ThrottleDecision::Emit { suppressed: 7 }
        );
        assert_eq!(
            throttle.admit("key", next_window),
            ThrottleDecision::Emit { suppressed: 0 }
        );
    }

    #[test]
    fn keys_do_not_share_a_budget() {
        let mut throttle = throttle();
        let now = Instant::now();

        for _ in 0..MAX_PER_WINDOW {
            throttle.admit("first", now);
        }
        assert_eq!(throttle.admit("first", now), ThrottleDecision::Suppress);
        assert_eq!(
            throttle.admit("second", now),
            ThrottleDecision::Emit { suppressed: 0 }
        );
    }

    #[test]
    fn a_window_ends_exactly_one_window_after_it_started() {
        let mut throttle = EventThrottle::new(WINDOW, 1, MAX_KEYS);
        let start = Instant::now();

        assert_eq!(
            throttle.admit("key", start),
            ThrottleDecision::Emit { suppressed: 0 }
        );
        // Still inside the window one millisecond before it elapses.
        assert_eq!(
            throttle.admit("key", start + WINDOW - Duration::from_millis(1)),
            ThrottleDecision::Suppress
        );
        // At exactly `start + WINDOW` the window has elapsed.
        assert_eq!(
            throttle.admit("key", start + WINDOW),
            ThrottleDecision::Emit { suppressed: 1 }
        );
    }

    #[test]
    fn keys_beyond_the_table_cap_fold_into_a_shared_overflow_window() {
        let mut throttle = EventThrottle::new(WINDOW, MAX_PER_WINDOW, 2);
        let now = Instant::now();

        throttle.admit("a", now);
        throttle.admit("b", now);
        // The table is full, so these distinct keys all land in the shared
        // overflow window and exhaust its budget together rather than each
        // getting a window of their own.
        for _ in 0..MAX_PER_WINDOW {
            assert!(matches!(
                throttle.admit(&format!("overflow-{}", next_id()), now),
                ThrottleDecision::Emit { .. }
            ));
        }
        assert_eq!(
            throttle.admit("yet-another", now),
            ThrottleDecision::Suppress
        );
        assert!(throttle.windows.contains_key(OVERFLOW_KEY));
        assert_eq!(
            throttle.windows.len(),
            3,
            "the cap plus the overflow bucket"
        );
    }

    /// Distinct key suffixes without pulling in a RNG dependency.
    fn next_id() -> u64 {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    #[test]
    fn an_idle_key_is_reclaimed_but_one_owing_a_suppressed_count_is_kept() {
        let mut throttle = EventThrottle::new(WINDOW, 1, MAX_KEYS);
        let start = Instant::now();

        // `idle` emits once and then goes quiet; `owing` overshoots its cap so
        // it still has a count to report.
        throttle.admit("idle", start);
        throttle.admit("owing", start);
        throttle.admit("owing", start);

        let much_later = start + WINDOW * 5;
        assert_eq!(
            throttle.admit("unrelated", much_later),
            ThrottleDecision::Emit { suppressed: 0 }
        );
        assert!(!throttle.windows.contains_key("idle"));
        assert_eq!(
            throttle.admit("owing", much_later),
            ThrottleDecision::Emit { suppressed: 1 },
            "a reclaimed key would have lost its suppressed count"
        );
    }
}
