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
/// [`WINDOW`].
const IDLE_WINDOWS: u32 = 2;

/// The same, for a key that still owes a suppressed count. That count is the
/// payload of the next event the key emits, so it outlives an ordinary idle
/// key by a wide margin — but not forever, or a key that never returns would
/// hold a table slot for the life of the process. Once even this elapses the
/// entry is reclaimed and what it owed is forfeited into
/// [`EventThrottle::forfeited`], to be carried by the next event admitted for
/// *any* key, so the count is still reported rather than silently dropped.
const DEBT_IDLE_WINDOWS: u32 = 60;

/// Whether one event may be sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ThrottleDecision {
    /// Send it. `suppressed` is how many events were dropped since the last
    /// one that was sent: this key's own count, non-zero only when a window
    /// just rolled over after hitting the cap, plus anything forfeited by a
    /// key reclaimed before it could report its own (see
    /// [`DEBT_IDLE_WINDOWS`]).
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
    /// Suppressed counts owed by keys that lapsed before reporting them. Not
    /// attributable to any live key, so the next event admitted for whichever
    /// key comes first carries them (see [`DEBT_IDLE_WINDOWS`]).
    forfeited: u64,
}

impl EventThrottle {
    pub(super) fn new(window: Duration, max_per_window: u32, max_keys: usize) -> Self {
        Self {
            window,
            max_per_window,
            max_keys,
            windows: HashMap::new(),
            forfeited: 0,
        }
    }

    /// Decide whether the event identified by `key` may be sent at `now`.
    pub(super) fn admit(&mut self, key: &str, now: Instant) -> ThrottleDecision {
        self.reclaim_idle(now);
        // The shared bucket is not one of the `max_keys` tracked slots, or a
        // table that overflowed once would carry a permanently reduced
        // capacity and never hand a reclaimed slot to a new key.
        let tracked = self.windows.len() - usize::from(self.windows.contains_key(OVERFLOW_KEY));
        let key = if self.windows.contains_key(key) || tracked < self.max_keys {
            key
        } else {
            OVERFLOW_KEY
        };
        if key != OVERFLOW_KEY {
            self.drain_finished_overflow_window(now);
        }

        let decision = match self.windows.get_mut(key) {
            Some(window) if now.saturating_duration_since(window.started_at) < self.window => {
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
        };

        match decision {
            // Anything a lapsed key never got to report rides out on the first
            // event actually admitted after it was forfeited; a suppressed
            // event carries nothing, so the debt stays pending.
            ThrottleDecision::Emit { suppressed } => ThrottleDecision::Emit {
                suppressed: suppressed.saturating_add(std::mem::take(&mut self.forfeited)),
            },
            ThrottleDecision::Suppress => ThrottleDecision::Suppress,
        }
    }

    fn reclaim_idle(&mut self, now: Instant) {
        let idle_after = self.window * IDLE_WINDOWS;
        let debt_idle_after = self.window * DEBT_IDLE_WINDOWS;
        let mut forfeited: u64 = 0;
        self.windows.retain(|_, window| {
            let idle_for = now.saturating_duration_since(window.started_at);
            if window.suppressed == 0 {
                return idle_for < idle_after;
            }
            if idle_for < debt_idle_after {
                return true;
            }
            forfeited = forfeited.saturating_add(window.suppressed);
            false
        });
        self.forfeited = self.forfeited.saturating_add(forfeited);
    }

    /// Hand the shared bucket's finished window to the pending count when an
    /// event is about to bypass the bucket. The bucket reports what it dropped
    /// on the next event it admits, but a key holding a tracked slot never
    /// reaches it — so after a burst that overflows and then subsides, the
    /// count would otherwise wait for the next burst or for the bucket to
    /// lapse [`DEBT_IDLE_WINDOWS`] later. Only a window that has already
    /// elapsed is taken: a current one is still counting, exactly as it is for
    /// any other key.
    fn drain_finished_overflow_window(&mut self, now: Instant) {
        let Some(window) = self.windows.get_mut(OVERFLOW_KEY) else {
            return;
        };
        if window.suppressed == 0 || now.saturating_duration_since(window.started_at) < self.window
        {
            return;
        }
        let suppressed = std::mem::take(&mut window.suppressed);
        self.forfeited = self.forfeited.saturating_add(suppressed);
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

    use super::{
        EventThrottle, ThrottleDecision, DEBT_IDLE_WINDOWS, MAX_KEYS, MAX_PER_WINDOW, OVERFLOW_KEY,
        WINDOW,
    };

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
    fn a_key_owing_a_suppressed_count_gives_its_debt_up_when_it_is_reclaimed() {
        // The debt exemption is generous but bounded: a key that never comes
        // back must not hold a table slot for the life of the process. What it
        // owes is handed to the next event admitted rather than disappearing.
        let mut throttle = EventThrottle::new(WINDOW, 1, MAX_KEYS);
        let start = Instant::now();

        throttle.admit("owing", start);
        for _ in 0..4 {
            assert_eq!(throttle.admit("owing", start), ThrottleDecision::Suppress);
        }

        // Just inside the horizon the key is still there and hands over its
        // debt directly.
        let within_horizon = start + WINDOW * (DEBT_IDLE_WINDOWS - 1);
        assert_eq!(
            throttle.admit("owing", within_horizon),
            ThrottleDecision::Emit { suppressed: 4 }
        );
        for _ in 0..3 {
            assert_eq!(
                throttle.admit("owing", within_horizon),
                ThrottleDecision::Suppress
            );
        }

        // Now let it lapse past the horizon with those three uncollected.
        let long_gone = within_horizon + WINDOW * DEBT_IDLE_WINDOWS;
        assert_eq!(
            throttle.admit("unrelated", long_gone),
            ThrottleDecision::Emit { suppressed: 3 },
            "the forfeited count is reported, not silently dropped"
        );
        assert!(
            !throttle.windows.contains_key("owing"),
            "the lapsed key must not hold its slot forever"
        );
    }

    #[test]
    fn a_forfeited_debt_resurfaces_on_the_next_admitted_event() {
        // Same reclamation, observed from the outside: the count reappears on
        // the next event any key gets through — which key carries it is not
        // load-bearing, the count is an aggregate signal.
        let mut throttle = EventThrottle::new(WINDOW, 1, 1);
        let start = Instant::now();

        throttle.admit("owing", start);
        throttle.admit("owing", start);
        throttle.admit("owing", start);

        let long_gone = start + WINDOW * (DEBT_IDLE_WINDOWS + 1);
        // Reclaiming `owing` frees the one tracked slot, so this distinct key
        // takes it — and collects the forfeited count on the way in.
        assert_eq!(
            throttle.admit("some-other-key", long_gone),
            ThrottleDecision::Emit { suppressed: 2 }
        );
    }

    #[test]
    fn a_slot_freed_by_reclamation_goes_to_a_new_key_even_once_the_bucket_exists() {
        let mut throttle = EventThrottle::new(WINDOW, MAX_PER_WINDOW, 2);
        let start = Instant::now();

        throttle.admit("a", start);
        throttle.admit("b", start);

        // Restart `b`'s window so it stays live; `a` goes quiet for good. Then
        // load the shared bucket while the table is still full, overshooting
        // its cap so it owes a count.
        let load = start + WINDOW + WINDOW / 2;
        throttle.admit("b", load);
        for _ in 0..MAX_PER_WINDOW + 1 {
            throttle.admit(&format!("overflow-{}", next_id()), load);
        }
        assert!(throttle.windows.contains_key(OVERFLOW_KEY));

        // `a` has now been idle long enough to be reclaimed and `b` has not, so
        // one of the two tracked slots is free next to the bucket. Counting the
        // bucket as a tracked key would make the table look full here and send
        // `fresh` into the bucket, whose current window is already at its cap —
        // a `Suppress` rather than a window of its own.
        let later = start + WINDOW * 2;
        assert_eq!(
            throttle.admit("fresh", later),
            ThrottleDecision::Emit { suppressed: 0 }
        );
        assert!(throttle.windows.contains_key("fresh"));
        assert!(throttle.windows.contains_key("b"));
        assert!(throttle.windows.contains_key(OVERFLOW_KEY));
    }

    #[test]
    fn the_buckets_finished_window_is_reported_when_a_tracked_key_bypasses_it() {
        // Nothing routes through the bucket once a tracked slot is free, so its
        // finished window has to be handed over or the count waits for the next
        // burst — or for the bucket to lapse an hour later.
        let mut throttle = EventThrottle::new(WINDOW, 1, 2);
        let start = Instant::now();

        throttle.admit("a", start);
        throttle.admit("b", start);
        // The table is full, so these fold into the bucket and overshoot it.
        throttle.admit("overflow-one", start);
        for _ in 0..2 {
            assert_eq!(
                throttle.admit(&format!("overflow-{}", next_id()), start),
                ThrottleDecision::Suppress
            );
        }

        // A tracked key emits once the bucket's window has elapsed. It never
        // touches the bucket, so it carries what the bucket dropped.
        let next_window = start + WINDOW + WINDOW / 2;
        assert_eq!(
            throttle.admit("b", next_window),
            ThrottleDecision::Emit { suppressed: 2 }
        );
        assert_eq!(
            throttle
                .windows
                .get(OVERFLOW_KEY)
                .map(|window| window.suppressed),
            Some(0),
            "the bucket must not report the same count again"
        );
    }

    #[test]
    fn a_forfeited_debt_survives_a_suppress_and_lands_on_the_next_emit() {
        let mut throttle = EventThrottle::new(WINDOW, 1, MAX_KEYS);
        let start = Instant::now();

        throttle.admit("owing", start);
        for _ in 0..3 {
            assert_eq!(throttle.admit("owing", start), ThrottleDecision::Suppress);
        }

        // `carrier` opens a window that is still current when `owing` lapses,
        // so the very call that forfeits the debt is itself suppressed.
        let lapse = start + WINDOW * DEBT_IDLE_WINDOWS;
        throttle.admit("carrier", lapse - WINDOW / 2);
        assert_eq!(throttle.admit("carrier", lapse), ThrottleDecision::Suppress);
        assert!(!throttle.windows.contains_key("owing"));

        assert_eq!(
            throttle.admit("fresh", lapse),
            ThrottleDecision::Emit { suppressed: 3 },
            "the debt is held until an event is actually admitted"
        );
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

    #[test]
    fn a_keys_own_rollover_count_and_forfeited_debt_combine_on_one_emit() {
        // The two sources of a suppressed count are independent and add up:
        // reporting only one of them would lose the other.
        let mut throttle = EventThrottle::new(WINDOW, 1, MAX_KEYS);
        let start = Instant::now();

        // `owing` runs up a count of 2 and then never comes back.
        throttle.admit("owing", start);
        for _ in 0..2 {
            assert_eq!(throttle.admit("owing", start), ThrottleDecision::Suppress);
        }

        // `carrier` runs up a count of 3 of its own, one window short of the
        // horizon so `owing` is still holding its slot.
        let late = start + WINDOW * (DEBT_IDLE_WINDOWS - 1);
        throttle.admit("carrier", late);
        for _ in 0..3 {
            assert_eq!(throttle.admit("carrier", late), ThrottleDecision::Suppress);
        }

        // One window on, `owing` lapses and forfeits its 2 in the same call
        // that rolls `carrier`'s window over on its own 3.
        let lapse = start + WINDOW * DEBT_IDLE_WINDOWS;
        assert_eq!(
            throttle.admit("carrier", lapse),
            ThrottleDecision::Emit { suppressed: 5 },
            "the key's own rollover count and the forfeited debt both count"
        );
        assert!(!throttle.windows.contains_key("owing"));

        // Neither source is reported twice. `carrier`'s cap is one per window,
        // so the next event it may emit is a window later.
        assert_eq!(
            throttle.admit("carrier", lapse + WINDOW),
            ThrottleDecision::Emit { suppressed: 0 }
        );
    }

    #[test]
    fn debts_forfeited_in_one_reclaim_pass_are_summed() {
        let mut throttle = EventThrottle::new(WINDOW, 1, MAX_KEYS);
        let start = Instant::now();

        // Two keys owing 2 and 3, lapsing together so a single reclaim pass
        // has to add them rather than keep only the last one it saw.
        throttle.admit("owing-a", start);
        for _ in 0..2 {
            assert_eq!(throttle.admit("owing-a", start), ThrottleDecision::Suppress);
        }
        throttle.admit("owing-b", start);
        for _ in 0..3 {
            assert_eq!(throttle.admit("owing-b", start), ThrottleDecision::Suppress);
        }

        let lapse = start + WINDOW * DEBT_IDLE_WINDOWS;
        assert_eq!(
            throttle.admit("fresh", lapse),
            ThrottleDecision::Emit { suppressed: 5 }
        );
        assert!(!throttle.windows.contains_key("owing-a"));
        assert!(!throttle.windows.contains_key("owing-b"));
    }
}
