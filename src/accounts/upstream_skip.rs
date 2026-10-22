//! Memory-only skip windows for upstreams that refused the account (issue #817).
//!
//! An upstream that answered with an account-level refusal (a spend limit, an
//! empty credit balance, a billing error) will refuse every request until the
//! cause clears, so the chain stops contacting it for a window instead of
//! paying one refused round trip per request. Entries are keyed by the
//! configured upstream (provider) name, so the map is bounded by config, and
//! an expired entry is dropped the next time the map is touched. Nothing is
//! persisted: a restart forgets every window.
//!
//! The clock is [`tokio::time::Instant`], which is the std monotonic clock
//! unless a test pauses the runtime, so the window boundaries are testable
//! with paused time.

use std::{collections::HashMap, sync::Mutex, time::Duration};

use tokio::time::Instant;

/// Upstream name → the instant its skip window ends.
#[derive(Debug, Default)]
pub(crate) struct UpstreamSkips {
    until: Mutex<HashMap<String, Instant>>,
}

impl UpstreamSkips {
    /// Skip `upstream` for `skip_for` from now, and return how long the
    /// recorded window now lasts. A window already running past the new one
    /// is kept, so concurrent refusals never shorten a skip.
    pub(crate) fn record(&self, upstream: &str, skip_for: Duration) -> Duration {
        let now = Instant::now();
        let candidate = now + skip_for;
        let mut until = self.until.lock().expect("upstream skip lock poisoned");
        until.retain(|_, end| *end > now);
        let end = until
            .entry(upstream.to_string())
            .and_modify(|end| *end = (*end).max(candidate))
            .or_insert(candidate);
        end.saturating_duration_since(now)
    }

    /// Whether `upstream` is inside an unexpired skip window.
    pub(crate) fn is_skipped(&self, upstream: &str) -> bool {
        let now = Instant::now();
        let mut until = self.until.lock().expect("upstream skip lock poisoned");
        match until.get(upstream) {
            Some(end) if *end > now => true,
            Some(_) => {
                until.remove(upstream);
                false
            }
            None => false,
        }
    }

    /// Drop the items whose upstream is being skipped, keeping the order of
    /// the rest. When every item would be dropped the list comes back
    /// unchanged: a request is still worth trying in configured order rather
    /// than refused by the gateway without contacting anyone.
    pub(crate) fn retain_unskipped<T>(
        &self,
        items: Vec<T>,
        upstream: impl Fn(&T) -> &str,
    ) -> Vec<T> {
        let skipped: Vec<bool> = items
            .iter()
            .map(|item| self.is_skipped(upstream(item)))
            .collect();
        if !skipped.contains(&true) || !skipped.contains(&false) {
            return items;
        }
        items
            .into_iter()
            .zip(skipped)
            .filter_map(|(item, skipped)| (!skipped).then_some(item))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::UpstreamSkips;

    #[test]
    fn a_fresh_state_skips_nothing() {
        let skips = UpstreamSkips::default();

        assert!(!skips.is_skipped("primary"));
        assert_eq!(
            skips.retain_unskipped(vec!["primary", "secondary"], |name| name),
            vec!["primary", "secondary"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_window_skips_until_it_ends_then_admits_the_upstream_again() {
        let skips = UpstreamSkips::default();
        skips.record("primary", Duration::from_secs(90));

        assert!(skips.is_skipped("primary"));
        assert!(!skips.is_skipped("secondary"));
        tokio::time::advance(Duration::from_secs(89)).await;
        assert!(skips.is_skipped("primary"), "still inside the window");
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!skips.is_skipped("primary"), "the window has ended");
    }

    #[tokio::test(start_paused = true)]
    async fn a_shorter_later_refusal_keeps_the_longer_window() {
        let skips = UpstreamSkips::default();
        skips.record("primary", Duration::from_secs(600));
        tokio::time::advance(Duration::from_secs(100)).await;

        let lasts = skips.record("primary", Duration::from_secs(10));

        assert_eq!(lasts, Duration::from_secs(500));
        tokio::time::advance(Duration::from_secs(499)).await;
        assert!(skips.is_skipped("primary"));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!skips.is_skipped("primary"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_longer_later_refusal_extends_the_window() {
        let skips = UpstreamSkips::default();
        skips.record("primary", Duration::from_secs(10));

        let lasts = skips.record("primary", Duration::from_secs(600));

        assert_eq!(lasts, Duration::from_secs(600));
    }

    #[test]
    fn skipped_items_are_dropped_and_the_rest_keep_their_order() {
        let skips = UpstreamSkips::default();
        skips.record("b", Duration::from_secs(60));

        assert_eq!(
            skips.retain_unskipped(vec!["a", "b", "c"], |name| name),
            vec!["a", "c"]
        );
    }

    #[test]
    fn every_item_skipped_keeps_the_configured_list() {
        let skips = UpstreamSkips::default();
        skips.record("a", Duration::from_secs(60));
        skips.record("b", Duration::from_secs(60));

        assert_eq!(
            skips.retain_unskipped(vec!["a", "b"], |name| name),
            vec!["a", "b"]
        );
    }
}
