//! Process-lifetime spend meter.
//!
//! Owned by [`SpendStore`](super::SpendStore) but deliberately separate from
//! `SpendState`: the state is cloned wholesale on every read, so per-request
//! counters live here behind their own short-held lock.
//!
//! Money is femto-USD in `u64` (see [`super::pricing`]); a spend-limit `amount`
//! is whole US cents, 1e13 femto-USD each. Counters are keyed by
//! `(principal, period, window start)` and saturate rather than wrap.

mod binding;
pub mod persist;
mod window;

#[cfg(test)]
mod tests;

use std::{
    collections::{HashMap, HashSet},
    sync::Mutex,
};

use super::{
    pricing::{PriceTable, Rates, Usage},
    store::{Period, Scope, SpendLimit},
};
pub use binding::{Binding, Threshold};
pub use window::{months_back_start, reset_label, window, Window};

/// The current time in Unix seconds (0 if the clock is before the epoch).
pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Reserved principal for requests with no client identity whose admission
/// envelope injects a credential.
pub const ANONYMOUS_PRINCIPAL: &str = "shunt:anonymous";

/// Femto-USD per US cent.
pub const FEMTO_USD_PER_CENT: u128 = 10_000_000_000_000;

/// Periods in the order the meter records them.
pub const PERIODS: [Period; 3] = [Period::Daily, Period::Weekly, Period::Monthly];

/// What a request with no resolvable price costs, USD per million tokens.
const UNKNOWN_INPUT: f64 = 5.0;
const UNKNOWN_OUTPUT: f64 = 25.0;
const UNKNOWN_CACHE_READ: f64 = 0.50;
const UNKNOWN_CACHE_WRITE: f64 = 6.25;

/// Outcome of [`SpendMeter::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    Allow,
    /// A reached cap blocks; when several are reached, the one that resets last.
    Blocked {
        period: Period,
        reset_at: u64,
    },
    /// The principal's counters could not be trusted (see [`SpendMeter::mark_unavailable`]).
    Unavailable,
}

/// Outcome of [`SpendMeter::assess`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Assessment {
    pub check: Check,
    /// The cap the principal's rate-limit headers describe; `None` when the
    /// principal has no cap in any period.
    pub binding: Option<Binding>,
}

/// One request's billable usage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestUsage {
    pub tokens: Usage,
    pub web_search_requests: u64,
}

/// How many distinct unpriced model ids [`SpendMeter`] remembers having warned
/// about. Model ids come from requests, so the set is capped rather than
/// allowed to grow with every id a client sends; ids past the cap are metered
/// silently.
const MAX_WARNED_MODELS: usize = 256;

/// Counters by principal, then by `(period, window start)`: a lookup borrows
/// the principal as `&str`, and one lock read covers every period.
type Counters = HashMap<String, HashMap<(Period, u64), u64>>;

#[derive(Default)]
pub struct SpendMeter {
    counters: Mutex<Counters>,
    /// Flagged principals. The value is the instant (Unix seconds) after which
    /// the principal is trusted again; `None` stays flagged until
    /// [`SpendMeter::clear_unavailable`].
    unavailable: Mutex<HashMap<String, Option<u64>>>,
    persist: persist::PersistState,
    warned_models: Mutex<HashSet<String>>,
}

impl SpendMeter {
    /// Adds `cost` femto-USD to the principal's daily, weekly and monthly
    /// windows for `now_secs`, under one lock acquisition.
    pub fn record(&self, principal: &str, now_secs: u64, cost: u64) {
        let mut counters = self.counters.lock().expect("spend meter lock poisoned");
        if !counters.contains_key(principal) {
            counters.insert(principal.to_string(), HashMap::new());
        }
        let windows = counters
            .get_mut(principal)
            .expect("the principal's counters were just ensured");
        for period in PERIODS {
            let slot = windows
                .entry((period, window(period, now_secs).start))
                .or_insert(0);
            *slot = slot.saturating_add(cost);
        }
        self.persist.mark_changed();
    }

    /// Period-to-date spend of `principal` in femto-USD.
    pub fn spent(&self, principal: &str, period: Period, now_secs: u64) -> u64 {
        self.spent_in(principal, &[(period, window(period, now_secs).start)])[0]
    }

    /// The spend in each of `windows` (`(period, start)`), read under one lock.
    fn spent_in<const N: usize>(&self, principal: &str, windows: &[(Period, u64); N]) -> [u64; N] {
        let counters = self.counters.lock().expect("spend meter lock poisoned");
        let held = counters.get(principal);
        windows.map(|key| held.and_then(|held| held.get(&key)).copied().unwrap_or(0))
    }

    /// Every principal with a counter in any retained window, ascending.
    pub fn principals(&self) -> Vec<String> {
        let counters = self.counters.lock().expect("spend meter lock poisoned");
        let mut principals: Vec<String> = counters.keys().cloned().collect();
        principals.sort();
        principals
    }

    /// Flags a principal whose persisted counters could not be read.
    pub fn mark_unavailable(&self, principal: &str) {
        self.unavailable
            .lock()
            .expect("spend meter lock poisoned")
            .entry(principal.to_string())
            .or_insert(None);
    }

    /// [`Self::mark_unavailable`] that lifts itself once `until` (Unix
    /// seconds) has passed, i.e. once every window the unreadable record could
    /// have covered has elapsed. A later, longer mark wins.
    pub fn mark_unavailable_until(&self, principal: &str, until: u64) {
        let mut flagged = self.unavailable.lock().expect("spend meter lock poisoned");
        let slot = flagged.entry(principal.to_string()).or_insert(None);
        *slot = Some(slot.map_or(until, |current| current.max(until)));
    }

    /// Clears the flag, e.g. once the poisoned windows have elapsed.
    pub fn clear_unavailable(&self, principal: &str) {
        self.unavailable
            .lock()
            .expect("spend meter lock poisoned")
            .remove(principal);
    }

    /// True while `principal` is flagged; a time-boxed flag whose deadline has
    /// passed at `now_secs` is cleared on the way.
    fn is_unavailable(&self, principal: &str, now_secs: u64) -> bool {
        let mut flagged = self.unavailable.lock().expect("spend meter lock poisoned");
        match flagged.get(principal) {
            None => false,
            Some(None) => true,
            Some(Some(until)) if now_secs < *until => true,
            Some(Some(_)) => {
                flagged.remove(principal);
                false
            }
        }
    }

    /// Decides whether `principal` may spend, given the stage-1 `limits`.
    #[cfg(test)]
    pub fn check(&self, limits: &[SpendLimit], principal: &str, now_secs: u64) -> Check {
        self.assess(limits, principal, now_secs).check
    }

    /// [`Self::check`] together with the binding cap the rate-limit headers
    /// describe, both from one read of each counter so they cannot disagree.
    ///
    /// When any cap is reached, `check` is `Blocked` on exactly the binding
    /// cap's period and reset. `binding` is `None` only for a principal with
    /// no cap in any period; it is still computed for an unavailable
    /// principal, where only its presence is meaningful.
    pub fn assess(&self, limits: &[SpendLimit], principal: &str, now_secs: u64) -> Assessment {
        let windows = PERIODS.map(|period| (period, window(period, now_secs)));
        let spent = self.spent_in(principal, &windows.map(|(period, w)| (period, w.start)));
        let binding = binding::fold(windows.into_iter().zip(spent).filter_map(
            |((period, window), spent)| {
                let cap = effective_cap(limits, principal, period)?;
                Some(Binding {
                    period,
                    spent: u128::from(spent),
                    cap,
                    reset_at: window.end,
                })
            },
        ));
        let check = if self.is_unavailable(principal, now_secs) {
            Check::Unavailable
        } else {
            match binding {
                Some(binding) if binding.exceeded() => Check::Blocked {
                    period: binding.period,
                    reset_at: binding.reset_at,
                },
                _ => Check::Allow,
            }
        };
        Assessment { check, binding }
    }

    /// Prices one request in femto-USD. An unpriceable model is charged the
    /// unknown-model rate (scaled by the table's multiplier) and warned about
    /// once per model id per process.
    pub fn cost(
        &self,
        table: &PriceTable,
        provider: &str,
        client_model: &str,
        upstream_model: &str,
        usage: &RequestUsage,
    ) -> u64 {
        let rates = table
            .resolve(provider, client_model, upstream_model)
            .unwrap_or_else(|| {
                self.warn_unknown_model(provider, client_model, upstream_model);
                table.apply_multiplier(&unknown_model_rates())
            });
        rates.cost_femto_usd(&usage.tokens).saturating_add(
            table
                .web_search_cost_femto_usd()
                .saturating_mul(usage.web_search_requests),
        )
    }

    /// True exactly once per model id for the life of the process, for the
    /// first [`MAX_WARNED_MODELS`] distinct ids. Once that many are held, a
    /// further id is neither remembered nor warned about; it is still priced
    /// at the unknown-model rate.
    fn first_sighting(&self, id: &str) -> bool {
        let mut warned = self
            .warned_models
            .lock()
            .expect("spend meter lock poisoned");
        if warned.len() >= MAX_WARNED_MODELS {
            return false;
        }
        warned.insert(id.to_string())
    }

    fn warn_unknown_model(&self, provider: &str, client_model: &str, upstream_model: &str) {
        let id = if upstream_model.is_empty() {
            client_model
        } else {
            upstream_model
        };
        if self.first_sighting(id) {
            tracing::warn!(
                provider,
                model = id,
                "no price for model; metering at the unknown-model rate"
            );
        }
    }
}

/// The rate charged for a model no override or catalog row prices, before the
/// table's multiplier.
pub fn unknown_model_rates() -> Rates {
    Rates::from_usd_per_million(
        UNKNOWN_INPUT,
        UNKNOWN_OUTPUT,
        UNKNOWN_CACHE_READ,
        UNKNOWN_CACHE_WRITE,
    )
}

/// The cap in femto-USD that binds `principal` for `period`: its own `user`
/// cap if one exists, else the `organization` cap, else `None` (unlimited).
/// An `amount: None` row is an explicit unlimited and still wins over the
/// organization row. The organization cap is a per-principal default, not a
/// shared pool.
pub fn effective_cap(limits: &[SpendLimit], principal: &str, period: Period) -> Option<u128> {
    let cents = effective_limit(limits, principal, period)?
        .amount
        .as_deref()?
        .parse::<u128>()
        .ok()?;
    Some(cents * FEMTO_USD_PER_CENT)
}

/// The stored row that decides [`effective_cap`]: the principal's own `user`
/// row if one exists (even an explicit unlimited), else the `organization` row.
pub fn effective_limit<'a>(
    limits: &'a [SpendLimit],
    principal: &str,
    period: Period,
) -> Option<&'a SpendLimit> {
    let find = |wanted: &dyn Fn(&Scope) -> bool| {
        limits
            .iter()
            .find(|limit| limit.period == period && wanted(&limit.scope))
    };
    find(&|scope| matches!(scope, Scope::User { user_id } if user_id == principal))
        .or_else(|| find(&|scope| matches!(scope, Scope::Organization)))
}
