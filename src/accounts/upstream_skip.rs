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

use super::{
    account_key, cap_exclusion, expire_stale_quota, governing_cooldown, unix_now, AccountPool,
};
use crate::config::{AccountConfig, PoolConfig};

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
    #[cfg(test)]
    pub(crate) fn is_skipped(&self, upstream: &str) -> bool {
        let mut until = self.until.lock().expect("upstream skip lock poisoned");
        skipped_at(&mut until, upstream, Instant::now())
    }

    /// Drop the items whose upstream is being skipped, keeping the order of
    /// the rest, and report whether the first item was among them. When every
    /// item would be dropped the list comes back unchanged (and the first is
    /// not reported dropped): a request is still worth trying in configured
    /// order rather than refused by the gateway without contacting anyone.
    /// `upstream` returns `None` for an item that is never skipped, whatever
    /// window its name carries. The lock is taken and the clock read once.
    pub(crate) fn retain_unskipped<T>(
        &self,
        items: Vec<T>,
        upstream: impl Fn(&T) -> Option<&str>,
    ) -> (Vec<T>, bool) {
        let now = Instant::now();
        let mut until = self.until.lock().expect("upstream skip lock poisoned");
        let mut is_skipped =
            |item: &T| upstream(item).is_some_and(|name| skipped_at(&mut until, name, now));
        let (mut any_skipped, mut any_kept) = (false, false);
        for item in &items {
            if is_skipped(item) {
                any_skipped = true;
            } else {
                any_kept = true;
            }
        }
        if !any_skipped || !any_kept {
            return (items, false);
        }
        let first_dropped = items.first().is_some_and(&mut is_skipped);
        let kept = items.into_iter().filter(|item| !is_skipped(item)).collect();
        (kept, first_dropped)
    }
}

/// Whether `upstream` is inside an unexpired window at `now`; an expired
/// entry is dropped on the way.
fn skipped_at(until: &mut HashMap<String, Instant>, upstream: &str, now: Instant) -> bool {
    match until.get(upstream) {
        Some(end) if *end > now => true,
        Some(_) => {
            until.remove(upstream);
            false
        }
        None => false,
    }
}

impl AccountPool {
    /// How long until the first of `accounts` can serve a request again: the
    /// shortest wait among the accounts selection may pick, i.e. neither
    /// disabled nor paused for `provider` (the same exclusions `select_order`
    /// applies). An account's wait is its remaining account-wide cooldown; a
    /// hard-capped account counts from when its cap clears (never less than
    /// its cooldown), and one whose cap has no known end is left out.
    /// Model-scoped and Fable-scoped cooldowns are ignored: they leave the
    /// account selectable for other requests, so they must not open an
    /// upstream-wide window. An account that is not cooling counts as zero.
    /// `None` when no account is selectable at all. The window a pool that
    /// every account refused is skipped for in multi-element chains (#817).
    pub(crate) fn earliest_reselect(
        &self,
        provider: &str,
        accounts: &[AccountConfig],
        pool: Option<&PoolConfig>,
    ) -> Option<Duration> {
        let now = std::time::Instant::now();
        let unix_now = unix_now();
        let entries = self.entries.lock().expect("account health lock poisoned");
        accounts
            .iter()
            .filter(|account| !account.disabled)
            .filter_map(
                |account| match entries.get(&account_key(provider, account)) {
                    None => Some(Duration::ZERO),
                    Some(health) if health.paused_providers.contains(provider) => None,
                    Some(health) => {
                        // Selection expires a stale observation before it
                        // reads the cap, so a lapsed cap does not hold the
                        // account out here either.
                        let mut quota = health.quota.clone();
                        expire_stale_quota(&mut quota, unix_now);
                        let cooling = governing_cooldown(health, false, None)
                            .unwrap_or(now)
                            .saturating_duration_since(now);
                        match cap_exclusion(&quota, account, false, pool) {
                            None => Some(cooling),
                            Some(cap) => cap.eligible_at.map(|at| {
                                Duration::from_secs(at.saturating_sub(unix_now)).max(cooling)
                            }),
                        }
                    }
                },
            )
            .min()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::UpstreamSkips;
    use crate::{
        accounts::{account_key, unix_now, AccountPool, QuotaState},
        config::AccountConfig,
    };

    fn pool_account(name: &str) -> AccountConfig {
        AccountConfig {
            name: name.to_string(),
            token_env: Some(format!("SHUNT_TEST_RESELECT_{name}")),
            ..Default::default()
        }
    }

    /// The pool's skip window is the shortest cooldown among its selectable
    /// accounts; an account not cooling makes it zero, and a disabled one
    /// does not count (#817).
    #[test]
    fn earliest_reselect_is_the_shortest_cooldown_among_selectable_accounts() {
        let pool = AccountPool::default();
        let (a, b) = (pool_account("a"), pool_account("b"));
        pool.cooldown("p", &a, Duration::from_secs(100), "test");
        pool.cooldown("p", &b, Duration::from_secs(300), "test");
        let both = [a.clone(), b.clone()];
        let window = pool.earliest_reselect("p", &both, None).unwrap();
        assert!(
            (Duration::from_secs(99)..=Duration::from_secs(100)).contains(&window),
            "{window:?}"
        );

        let fresh = pool_account("fresh");
        let window = pool.earliest_reselect("p", &[a.clone(), fresh.clone()], None);
        assert_eq!(window, Some(Duration::ZERO));

        let disabled = AccountConfig {
            disabled: true,
            ..fresh
        };
        let window = pool
            .earliest_reselect("p", &[b, disabled.clone()], None)
            .unwrap();
        assert!(window > Duration::from_secs(290), "{window:?}");
        assert_eq!(pool.earliest_reselect("p", &[disabled], None), None);
    }

    /// A hard-capped account is not selectable now, so it must not make the pool
    /// look immediately usable and keep the skip window from opening (#817).
    #[test]
    fn earliest_reselect_ignores_a_hard_capped_account() {
        let pool = AccountPool::default();
        let a = pool_account("a");
        let capped = AccountConfig {
            max_utilization: Some(0.5),
            ..pool_account("capped")
        };
        pool.cooldown("p", &a, Duration::from_secs(100), "test");
        let far = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        pool.import_quotas([(
            account_key("p", &capped),
            QuotaState {
                utilization_5h: Some(0.9),
                reset_5h: Some(far),
                ..Default::default()
            },
        )]);

        let window = pool.earliest_reselect("p", &[a.clone(), capped.clone()], None);

        assert!(window.unwrap() > Duration::from_secs(90), "{window:?}");
        // Alone, the capped account is no longer dropped: it counts from its reset.
        let alone = pool.earliest_reselect("p", &[capped], None).unwrap();
        assert!(alone > Duration::from_secs(3590), "{alone:?}");
    }

    /// A capped account counts from when its cap clears, not never: if one
    /// account refused for an hour and the other's cap resets in a minute, the
    /// pool is skipped for about a minute, not the hour (#817).
    #[test]
    fn earliest_reselect_counts_a_capped_account_from_its_cap_reset() {
        let pool = AccountPool::default();
        let a = pool_account("a");
        let capped = AccountConfig {
            max_utilization: Some(0.5),
            ..pool_account("capped")
        };
        pool.cooldown("p", &a, Duration::from_secs(3600), "test");
        pool.import_quotas([(
            account_key("p", &capped),
            QuotaState {
                utilization_5h: Some(0.9),
                reset_5h: Some(unix_now() + 60),
                ..Default::default()
            },
        )]);

        let window = pool.earliest_reselect("p", &[a, capped], None).unwrap();

        assert!(
            (Duration::from_secs(55)..=Duration::from_secs(60)).contains(&window),
            "{window:?}"
        );
    }

    /// A Fable-scoped cooldown leaves the account usable for other requests,
    /// so it does not open an upstream-wide window (#817).
    #[test]
    fn earliest_reselect_ignores_a_model_scoped_cooldown() {
        let pool = AccountPool::default();
        let (a, b) = (pool_account("a"), pool_account("b"));
        pool.cooldown_scoped(
            "p",
            &a,
            Duration::from_secs(600),
            "test",
            crate::accounts::CooldownScope::Fable,
        );
        pool.cooldown("p", &b, Duration::from_secs(300), "test");

        assert_eq!(
            pool.earliest_reselect("p", &[a, b], None),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn a_fresh_state_skips_nothing() {
        let skips = UpstreamSkips::default();

        assert!(!skips.is_skipped("primary"));
        assert_eq!(
            skips
                .retain_unskipped(vec!["primary", "secondary"], |name| Some(*name))
                .0,
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
            skips
                .retain_unskipped(vec!["a", "b", "c"], |name| Some(*name))
                .0,
            vec!["a", "c"]
        );
    }

    #[test]
    fn every_item_skipped_keeps_the_configured_list() {
        let skips = UpstreamSkips::default();
        skips.record("a", Duration::from_secs(60));
        skips.record("b", Duration::from_secs(60));

        assert_eq!(
            skips.retain_unskipped(vec!["a", "b"], |name| Some(*name)).0,
            vec!["a", "b"]
        );
    }
}
