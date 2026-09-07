//! Token pricing for the spend meter: a rate table resolved from
//! `[server.spend.pricing]` plus the arithmetic that turns a request's token
//! usage into a cost.
//!
//! Pure and side-effect free. Nothing calls [`PriceTable::resolve`] yet — the
//! meter that will is not implemented (see `docs/gateway-spend-limits.md`) —
//! so this module owns no state and reads no request.
//!
//! Money is carried as **nano-USD** (1e-9 USD) in `u64` rather than as a float:
//! rates are multiplied by token counts and summed across many requests, and a
//! float accumulator would drift against the whole-cent caps the spend API
//! stores. A rate is nano-USD *per token*, i.e. USD-per-million × 1000.

pub mod catalog;

use crate::config::{PricingConfig, PricingOverride};

use catalog::builtin_row;
pub use catalog::{canonical_builtin_id, LIST_PRICES, WEB_SEARCH_LIST_PRICE_NANO_USD};

/// One model's four rates, in nano-USD per token.
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
            input: nano_usd_per_token(input),
            output: nano_usd_per_token(output),
            cache_read: nano_usd_per_token(cache_read),
            cache_write: nano_usd_per_token(cache_write),
        }
    }

    /// Total cost of `usage` in nano-USD, saturating at `u64::MAX`.
    pub fn cost_nano_usd(&self, usage: &Usage) -> u64 {
        let total = u128::from(self.input) * u128::from(usage.input_tokens)
            + u128::from(self.output) * u128::from(usage.output_tokens)
            + u128::from(self.cache_read) * u128::from(usage.cache_read_input_tokens)
            + u128::from(self.cache_write) * u128::from(usage.cache_creation_input_tokens);
        u64::try_from(total).unwrap_or(u64::MAX)
    }

    /// Every rate scaled by `ppm` parts per million, rounding down.
    pub fn scaled(&self, ppm: u32) -> Self {
        Self {
            input: scale_nano_usd(self.input, ppm),
            output: scale_nano_usd(self.output, ppm),
            cache_read: scale_nano_usd(self.cache_read, ppm),
            cache_write: scale_nano_usd(self.cache_write, ppm),
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
    multiplier_ppm: u32,
    overrides: Vec<Override>,
}

impl PriceTable {
    /// `None` — no `[server.spend.pricing]` — means list prices at multiplier 1.
    pub fn from_config(pricing: Option<&PricingConfig>) -> Self {
        let Some(pricing) = pricing else {
            return Self {
                multiplier_ppm: ONE_MILLION,
                overrides: Vec::new(),
            };
        };
        Self {
            multiplier_ppm: multiplier_ppm(pricing.multiplier),
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
        Some(rates.scaled(self.multiplier_ppm))
    }

    /// The cost of one server-side web search, in nano-USD. Priced per request
    /// rather than per token, so only the multiplier applies.
    pub fn web_search_cost_nano_usd(&self) -> u64 {
        scale_nano_usd(WEB_SEARCH_LIST_PRICE_NANO_USD, self.multiplier_ppm)
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

fn resolve_override(row: &PricingOverride) -> Override {
    Override {
        upstream: row.upstream.clone(),
        model: row.model.trim().to_string(),
        canonical: canonical_builtin_id(&row.model),
        rates: Rates::from_usd_per_million(row.input, row.output, row.cache_read, row.cache_write),
    }
}

fn list_price(row: &'static (&'static str, f64, f64, f64, f64)) -> Rates {
    let (_, input, output, cache_read, cache_write) = row;
    Rates::from_usd_per_million(*input, *output, *cache_read, *cache_write)
}

/// The smallest multiplier that does not round every rate to zero, and the
/// smallest USD-per-million rate that does not quantize to zero nano-USD per
/// token. `Config::validate_pricing` rejects anything below these so a positive
/// number in the config can never silently price requests at $0.
pub const MIN_MULTIPLIER: f64 = 1.0 / ONE_MILLION as f64;
pub const MIN_USD_PER_MILLION: f64 = 0.001;

fn multiplier_ppm(multiplier: f64) -> u32 {
    if !multiplier.is_finite() || multiplier <= 0.0 || multiplier > 1.0 {
        return ONE_MILLION;
    }
    (multiplier * f64::from(ONE_MILLION)).round() as u32
}

fn nano_usd_per_token(usd_per_million: f64) -> u64 {
    if !usd_per_million.is_finite() || usd_per_million <= 0.0 {
        return 0;
    }
    (usd_per_million * 1_000.0).round() as u64
}

fn scale_nano_usd(value: u64, ppm: u32) -> u64 {
    let scaled = u128::from(value) * u128::from(ppm) / u128::from(ONE_MILLION);
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{PriceTable, Rates, Usage, WEB_SEARCH_LIST_PRICE_NANO_USD};
    use crate::config::{PricingConfig, PricingOverride};

    fn override_row(upstream: &str, model: &str, input: f64) -> PricingOverride {
        PricingOverride {
            upstream: upstream.into(),
            model: model.into(),
            input,
            output: input * 5.0,
            cache_read: input / 10.0,
            cache_write: input * 1.25,
        }
    }

    /// The rows are declared least-specific first, so a resolver that took the
    /// first matching row would return the built-in row for every lookup. The
    /// literal rows name an alias and an inference-profile ARN — strings that
    /// are not built-ins — because two spellings of one built-in on one
    /// upstream are a duplicate `Config::validate` rejects.
    #[test]
    fn resolve_prefers_the_most_specific_override_row_not_the_first_declared() {
        const ARN: &str = "arn:aws:bedrock:eu-west-1:123456789012:inference-profile/eu.anthropic.claude-sonnet-4-6-20260217-v1:0";
        let pricing = PricingConfig {
            multiplier: 1.0,
            overrides: vec![
                override_row("bedrock-eu", "claude-sonnet-4-6", 1.0),
                override_row("bedrock-eu", "sonnet-alias", 2.0),
                override_row("bedrock-eu", ARN, 3.0),
            ],
        };
        let config_rows = pricing.overrides.clone();
        let table = PriceTable::from_config(Some(&pricing));

        // The three rows must coexist in a bootable config: none canonicalizes
        // to the same key as another.
        let mut keys: Vec<String> = config_rows
            .iter()
            .map(|row| {
                super::canonical_builtin_id(&row.model)
                    .map(str::to_string)
                    .unwrap_or_else(|| row.model.to_ascii_lowercase())
            })
            .collect();
        keys.sort();
        keys.dedup();
        assert_eq!(
            keys.len(),
            3,
            "rows must not collide under duplicate detection"
        );

        // Upstream-model literal match beats the client-model literal match and
        // the canonical (built-in) match.
        let rates = table
            .resolve("bedrock-eu", "sonnet-alias", ARN)
            .expect("an override matches the upstream model");
        assert_eq!(rates.input, 3_000);

        // Client-model literal match beats the canonical match.
        let rates = table
            .resolve("bedrock-eu", "sonnet-alias", "claude-sonnet-4-6-20260217")
            .expect("an override matches the client model");
        assert_eq!(rates.input, 2_000);

        // Neither literal matches: the dated snapshot is the same built-in.
        let rates = table
            .resolve("bedrock-eu", "unknown-alias", "claude-sonnet-4-6-20260217")
            .expect("an override matches the canonical built-in");
        assert_eq!(rates.input, 1_000);

        // A different upstream sees none of those rows, only the list price.
        let rates = table
            .resolve("bedrock-us", "sonnet-alias", "claude-sonnet-4-6")
            .expect("the list price prices a built-in");
        assert_eq!(rates.input, 3_000);
        assert_eq!(rates.output, 15_000);
    }

    /// A built-in client id remapped to a non-Anthropic upstream model must
    /// not fall back to the Anthropic list price for that client id.
    #[test]
    fn list_price_keys_on_the_upstream_model_not_the_client_model() {
        let table = PriceTable::from_config(None);
        assert_eq!(table.resolve("codex", "claude-sonnet-4-6", "gpt-5.2"), None);
        assert!(table
            .resolve("codex", "gpt-5.2", "claude-sonnet-4-6")
            .is_some());
    }

    #[test]
    fn override_and_model_matching_are_case_insensitive() {
        let pricing = PricingConfig {
            multiplier: 1.0,
            overrides: vec![override_row("bedrock-eu", "Sonnet-Alias", 4.0)],
        };
        let table = PriceTable::from_config(Some(&pricing));

        let rates = table
            .resolve("bedrock-eu", "SONNET-alias", "unknown")
            .expect("the override matches regardless of case");
        assert_eq!(rates.input, 4_000);

        let rates = table
            .resolve("bedrock-us", "haiku", "CLAUDE-Haiku-4-5")
            .expect("the list price matches regardless of case");
        assert_eq!(rates.input, 1_000);
    }

    #[test]
    fn multiplier_scales_list_prices_overrides_and_web_search() {
        let pricing = PricingConfig {
            multiplier: 0.85,
            overrides: vec![override_row("bedrock-eu", "claude-opus-4-1", 10.0)],
        };
        let table = PriceTable::from_config(Some(&pricing));

        let list = table
            .resolve("bedrock-us", "claude-opus-4-1", "claude-opus-4-1")
            .expect("built-in");
        assert_eq!(list.input, 12_750); // 15 USD/M -> 15_000 nano -> x0.85
        assert_eq!(list.output, 63_750);

        let overridden = table
            .resolve("bedrock-eu", "claude-opus-4-1", "claude-opus-4-1")
            .expect("override");
        assert_eq!(overridden.input, 8_500); // 10 USD/M -> 10_000 nano -> x0.85

        assert_eq!(table.web_search_cost_nano_usd(), 8_500_000);

        let unscaled = PriceTable::from_config(None);
        assert_eq!(
            unscaled.web_search_cost_nano_usd(),
            WEB_SEARCH_LIST_PRICE_NANO_USD
        );
    }

    /// Scaling floors rather than rounding, so the meter never charges above the
    /// configured discount.
    #[test]
    fn scaling_rounds_down() {
        let rates = Rates {
            input: 3,
            ..Rates::default()
        };
        assert_eq!(rates.scaled(850_000).input, 2); // 2.55 -> 2
    }

    #[test]
    fn unknown_model_has_no_price() {
        let table = PriceTable::from_config(None);
        assert_eq!(table.resolve("bedrock-eu", "my-alias", "my-alias"), None);
    }

    #[test]
    fn cost_sums_the_four_token_classes_and_saturates() {
        let rates = Rates::from_usd_per_million(3.0, 15.0, 0.3, 3.75);
        let usage = Usage {
            input_tokens: 1_000,
            output_tokens: 200,
            cache_read_input_tokens: 10_000,
            cache_creation_input_tokens: 500,
        };
        // 1000*3000 + 200*15000 + 10000*300 + 500*3750 = 10_875_000 nano-USD
        assert_eq!(rates.cost_nano_usd(&usage), 10_875_000);

        let huge = Rates {
            input: u64::MAX,
            ..Rates::default()
        };
        assert_eq!(
            huge.cost_nano_usd(&Usage {
                input_tokens: 2,
                ..Usage::default()
            }),
            u64::MAX
        );
    }
}
