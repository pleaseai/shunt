//! Token pricing for the spend meter: a rate table resolved from
//! `[server.spend.pricing]` plus the arithmetic that turns a request's token
//! usage into a cost.
//!
//! Pure and side-effect free. Nothing calls [`PriceTable::resolve`] yet — the
//! meter that will is not implemented (see `docs/gateway-spend-limits.md`) — so
//! this module owns no state and reads no request.
//!
//! Money is carried as **femto-USD** (1e-15 USD) in `u64` rather than as a
//! float: rates are multiplied by token counts and summed across many requests,
//! and a float accumulator would drift against the whole-cent caps the spend API
//! stores. A rate is femto-USD *per token*, i.e. USD-per-million × 1e9.
//!
//! The unit has to be this fine because both configured floors apply at once:
//! [`MIN_USD_PER_MILLION`] × [`MIN_MULTIPLIER`] is exactly 1 femto-USD per
//! token, the smallest nonzero rate the type can carry, and a coarser unit —
//! nano-USD, say — quantizes that combination to zero, pricing a valid discount
//! at $0. The other end is [`MAX_USD_PER_MILLION`]: `u64::MAX` femto-USD is
//! about $18,446, per token and per request alike.

pub mod catalog;

use crate::config::{PricingConfig, PricingOverride};

use catalog::builtin_row;
pub use catalog::{canonical_builtin_id, LIST_PRICES, WEB_SEARCH_LIST_PRICE_FEMTO_USD};

/// One model's four rates, in femto-USD per token.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Rates {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// The token counts a priced request reports, named as the Anthropic Messages
/// usage block names them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

impl Rates {
    /// Converts the four USD-per-million-token rates the catalog and the config
    /// both state. A non-finite or non-positive rate becomes `0`; config
    /// validation rejects those before they reach here.
    pub fn from_usd_per_million(
        input: f64,
        output: f64,
        cache_read: f64,
        cache_write: f64,
    ) -> Self {
        Self {
            input: femto_usd_per_token(input),
            output: femto_usd_per_token(output),
            cache_read: femto_usd_per_token(cache_read),
            cache_write: femto_usd_per_token(cache_write),
        }
    }

    /// Total cost of `usage` in femto-USD, saturating at `u64::MAX`.
    ///
    /// Each product fits `u128`, but four of them summed do not, so the sum
    /// saturates too: `u64::MAX * u64::MAX` is already most of the `u128` range.
    pub fn cost_femto_usd(&self, usage: &Usage) -> u64 {
        let total = (u128::from(self.input) * u128::from(usage.input_tokens))
            .saturating_add(u128::from(self.output) * u128::from(usage.output_tokens))
            .saturating_add(u128::from(self.cache_read) * u128::from(usage.cache_read_input_tokens))
            .saturating_add(
                u128::from(self.cache_write) * u128::from(usage.cache_creation_input_tokens),
            );
        u64::try_from(total).unwrap_or(u64::MAX)
    }

    /// Every rate scaled by `ppb` parts per billion, rounding down.
    pub fn scaled(&self, ppb: u32) -> Self {
        Self {
            input: scale_femto_usd(self.input, ppb),
            output: scale_femto_usd(self.output, ppb),
            cache_read: scale_femto_usd(self.cache_read, ppb),
            cache_write: scale_femto_usd(self.cache_write, ppb),
        }
    }
}

/// A resolved `[[server.spend.pricing.overrides]]` row.
#[derive(Debug, Clone)]
struct Override {
    upstream: String,
    /// `model` as configured; matched ASCII-case-insensitively.
    model: String,
    /// `model` as a built-in catalog id, when it names one.
    canonical: Option<&'static str>,
    rates: Rates,
}

/// The rates the spend meter prices with: the built-in list prices, the
/// configured override rows layered over them, and one multiplier applied to
/// whatever wins.
#[derive(Debug, Clone)]
pub struct PriceTable {
    multiplier_ppb: u32,
    overrides: Vec<Override>,
}

impl PriceTable {
    /// `None` — no `[server.spend.pricing]` — means list prices at multiplier 1.
    pub fn from_config(pricing: Option<&PricingConfig>) -> Self {
        let Some(pricing) = pricing else {
            return Self {
                multiplier_ppb: ONE_BILLION,
                overrides: Vec::new(),
            };
        };
        Self {
            multiplier_ppb: multiplier_ppb(pricing.multiplier),
            overrides: pricing.overrides.iter().map(resolve_override).collect(),
        }
    }

    /// The rates for one request, with the multiplier already applied, or
    /// `None` when neither an override row nor the built-in catalog can price
    /// the model — the caller decides what an unpriceable request costs.
    ///
    /// Most specific wins: an override naming the upstream model, then one
    /// naming the client model, then one naming the same built-in by a
    /// different id (a dated snapshot, a Bedrock id), then the list price of
    /// the upstream model. The client model never selects a list price.
    pub fn resolve(
        &self,
        upstream: &str,
        client_model: &str,
        upstream_model: &str,
    ) -> Option<Rates> {
        // Canonicalizing allocates a normalized `String`, so do it once per
        // model rather than once per fallback rung.
        let upstream_row = builtin_row(upstream_model);
        let client_row = builtin_row(client_model);
        let rates = self
            .literal_override(upstream, upstream_model)
            .or_else(|| self.literal_override(upstream, client_model))
            .or_else(|| upstream_row.and_then(|(id, ..)| self.canonical_override(upstream, id)))
            .or_else(|| client_row.and_then(|(id, ..)| self.canonical_override(upstream, id)))
            // The list price keys on the model the upstream actually served:
            // a built-in client id remapped to a non-Anthropic upstream model
            // must not bill that provider's tokens at Anthropic rates.
            .or_else(|| upstream_row.map(list_price))?;
        Some(rates.scaled(self.multiplier_ppb))
    }

    /// The cost of one server-side web search, in femto-USD. Priced per request
    /// rather than per token, so only the multiplier applies.
    pub fn web_search_cost_femto_usd(&self) -> u64 {
        scale_femto_usd(WEB_SEARCH_LIST_PRICE_FEMTO_USD, self.multiplier_ppb)
    }

    fn literal_override(&self, upstream: &str, model: &str) -> Option<Rates> {
        let model = crate::routing::strip_context_window_hint(model.trim());
        self.overrides
            .iter()
            .find(|row| row.upstream == upstream && row.model.eq_ignore_ascii_case(model))
            .map(|row| row.rates)
    }

    fn canonical_override(&self, upstream: &str, canonical: &str) -> Option<Rates> {
        self.overrides
            .iter()
            .find(|row| row.upstream == upstream && row.canonical == Some(canonical))
            .map(|row| row.rates)
    }
}

const ONE_MILLION: u32 = 1_000_000;
const ONE_BILLION: u32 = 1_000_000_000;

fn resolve_override(row: &PricingOverride) -> Override {
    Override {
        upstream: row.upstream.clone(),
        // The stored key must be normalized exactly as `literal_override`
        // normalizes the request's model, or a row whose `model` carries the
        // `[1m]` context-window hint could never match: the lookup strips the
        // hint from the request and the stored key would still carry it.
        model: crate::routing::strip_context_window_hint(row.model.trim()).to_string(),
        canonical: canonical_builtin_id(&row.model),
        rates: Rates::from_usd_per_million(row.input, row.output, row.cache_read, row.cache_write),
    }
}

fn list_price(row: &'static (&'static str, f64, f64, f64, f64)) -> Rates {
    let (_, input, output, cache_read, cache_write) = row;
    Rates::from_usd_per_million(*input, *output, *cache_read, *cache_write)
}

/// The smallest multiplier and the smallest USD-per-million rate the config
/// accepts; `Config::validate_pricing` rejects anything below them.
///
/// They are the floors the femto-USD unit was chosen for: `MIN_USD_PER_MILLION`
/// is 1e6 femto-USD per token, which `MIN_MULTIPLIER` (one part per million)
/// scales to exactly 1 femto-USD per token — the smallest representable nonzero
/// rate — so both floors can be taken at once and the rate stays positive.
///
/// The floor is a property of that femto-USD product, not of the scale the
/// multiplier is stored at. Multipliers are carried as parts per *billion*,
/// which is a quantization and not an exact representation: an accepted value
/// is rounded to the nearest part per billion, so the applied discount can
/// differ from the configured one by at most 5e-10 in absolute terms. That
/// bound is worst in relative terms at `MIN_MULTIPLIER`, where it is 0.05%, and
/// shrinks proportionally as the multiplier rises.
///
/// The scale is parts per billion rather than per million because per-million
/// quantization was coarse enough to change the discount materially: it rounded
/// an accepted `0.0000014` down onto the `0.000001` floor — a 29% undercharge
/// the config never asked for — and an accepted `0.9999996` up to an
/// undiscounted `1.0`.
pub const MIN_MULTIPLIER: f64 = 1.0 / ONE_MILLION as f64;
pub const MIN_USD_PER_MILLION: f64 = 0.001;

/// The ceiling the same quantization imposes: the largest whole USD-per-million
/// rate whose femto-USD-per-token value still fits `u64`. Above it
/// `femto_usd_per_token` saturates, pricing at `u64::MAX` rather than at what
/// the config states.
pub const MAX_USD_PER_MILLION: f64 = 18_446_744_073.0;

fn multiplier_ppb(multiplier: f64) -> u32 {
    if !multiplier.is_finite() || multiplier <= 0.0 || multiplier > 1.0 {
        return ONE_BILLION;
    }
    (multiplier * f64::from(ONE_BILLION)).round() as u32
}

fn femto_usd_per_token(usd_per_million: f64) -> u64 {
    if !usd_per_million.is_finite() || usd_per_million <= 0.0 {
        return 0;
    }
    (usd_per_million * 1_000_000_000.0).round() as u64
}

fn scale_femto_usd(value: u64, ppb: u32) -> u64 {
    let scaled = u128::from(value) * u128::from(ppb) / u128::from(ONE_BILLION);
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
