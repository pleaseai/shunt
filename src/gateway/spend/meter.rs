//! Process-lifetime spend meter.
//!
//! Owned by [`SpendStore`](super::SpendStore) but deliberately separate from
//! `SpendState`: the state is cloned wholesale on every read, so per-request
//! counters live here behind their own short-held lock.
//!
//! Money is femto-USD in `u64` (see [`super::pricing`]); a spend-limit `amount`
//! is whole US cents, 1e13 femto-USD each. Counters are keyed by
//! `(principal, period, window start)` and saturate rather than wrap.

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
pub use window::{reset_label, window, Window};

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

/// One request's billable usage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestUsage {
    pub tokens: Usage,
    pub web_search_requests: u64,
}

type CounterKey = (String, Period, u64);

#[derive(Default)]
pub struct SpendMeter {
    counters: Mutex<HashMap<CounterKey, u64>>,
    unavailable: Mutex<HashSet<String>>,
    warned_models: Mutex<HashSet<String>>,
}

impl SpendMeter {
    /// Adds `cost` femto-USD to the principal's daily, weekly and monthly
    /// windows for `now_secs`, under one lock acquisition.
    pub fn record(&self, principal: &str, now_secs: u64, cost: u64) {
        let mut counters = self.counters.lock().expect("spend meter lock poisoned");
        for period in PERIODS {
            let key = (
                principal.to_string(),
                period,
                window(period, now_secs).start,
            );
            let slot = counters.entry(key).or_insert(0);
            *slot = slot.saturating_add(cost);
        }
    }

    /// Period-to-date spend of `principal` in femto-USD.
    pub fn spent(&self, principal: &str, period: Period, now_secs: u64) -> u64 {
        let key = (
            principal.to_string(),
            period,
            window(period, now_secs).start,
        );
        self.counters
            .lock()
            .expect("spend meter lock poisoned")
            .get(&key)
            .copied()
            .unwrap_or(0)
    }

    /// Flags a principal whose persisted counters could not be read.
    pub fn mark_unavailable(&self, principal: &str) {
        self.unavailable
            .lock()
            .expect("spend meter lock poisoned")
            .insert(principal.to_string());
    }

    /// Clears the flag, e.g. once the poisoned windows have elapsed.
    pub fn clear_unavailable(&self, principal: &str) {
        self.unavailable
            .lock()
            .expect("spend meter lock poisoned")
            .remove(principal);
    }

    /// Decides whether `principal` may spend, given the stage-1 `limits`.
    pub fn check(&self, limits: &[SpendLimit], principal: &str, now_secs: u64) -> Check {
        if self
            .unavailable
            .lock()
            .expect("spend meter lock poisoned")
            .contains(principal)
        {
            return Check::Unavailable;
        }
        let mut blocking: Option<(Period, u64)> = None;
        for period in PERIODS {
            let Some(cap) = effective_cap(limits, principal, period) else {
                continue;
            };
            if u128::from(self.spent(principal, period, now_secs)) < cap {
                continue;
            }
            let reset_at = window(period, now_secs).end;
            if blocking.is_none_or(|(_, latest)| reset_at > latest) {
                blocking = Some((period, reset_at));
            }
        }
        blocking.map_or(Check::Allow, |(period, reset_at)| Check::Blocked {
            period,
            reset_at,
        })
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

    /// True exactly once per model id for the life of the process.
    fn first_sighting(&self, id: &str) -> bool {
        self.warned_models
            .lock()
            .expect("spend meter lock poisoned")
            .insert(id.to_string())
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
    let find = |wanted: &dyn Fn(&Scope) -> bool| {
        limits
            .iter()
            .find(|limit| limit.period == period && wanted(&limit.scope))
    };
    let row = find(&|scope| matches!(scope, Scope::User { user_id } if user_id == principal))
        .or_else(|| find(&|scope| matches!(scope, Scope::Organization)))?;
    let cents = row.amount.as_deref()?.parse::<u128>().ok()?;
    Some(cents * FEMTO_USD_PER_CENT)
}
