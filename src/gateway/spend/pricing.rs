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

    /// Every rate scaled by `ppm` parts per million, rounding down.
    pub fn scaled(&self, ppm: u32) -> Self {
        Self {
            input: scale_femto_usd(self.input, ppm),
            output: scale_femto_usd(self.output, ppm),
            cache_read: scale_femto_usd(self.cache_read, ppm),
            cache_write: scale_femto_usd(self.cache_write, ppm),
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

    /// The cost of one server-side web search, in femto-USD. Priced per request
    /// rather than per token, so only the multiplier applies.
    pub fn web_search_cost_femto_usd(&self) -> u64 {
        scale_femto_usd(WEB_SEARCH_LIST_PRICE_FEMTO_USD, self.multiplier_ppm)
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

/// The smallest multiplier the parts-per-million scale can carry, and the
/// smallest USD-per-million rate the config accepts; `Config::validate_pricing`
/// rejects anything below them.
///
/// They are the floors the femto-USD unit was chosen for: `MIN_USD_PER_MILLION`
/// is 1e6 femto-USD per token, which `MIN_MULTIPLIER` (1 part per million)
/// scales to exactly 1 femto-USD per token — the smallest representable nonzero
/// rate — so both floors can be taken at once and the rate stays positive.
pub const MIN_MULTIPLIER: f64 = 1.0 / ONE_MILLION as f64;
pub const MIN_USD_PER_MILLION: f64 = 0.001;

/// The ceiling the same quantization imposes: the largest whole USD-per-million
/// rate whose femto-USD-per-token value still fits `u64`. Above it
/// `femto_usd_per_token` saturates, pricing at `u64::MAX` rather than at what
/// the config states.
pub const MAX_USD_PER_MILLION: f64 = 18_446_744_073.0;

fn multiplier_ppm(multiplier: f64) -> u32 {
    if !multiplier.is_finite() || multiplier <= 0.0 || multiplier > 1.0 {
        return ONE_MILLION;
    }
    (multiplier * f64::from(ONE_MILLION)).round() as u32
}

fn femto_usd_per_token(usd_per_million: f64) -> u64 {
    if !usd_per_million.is_finite() || usd_per_million <= 0.0 {
        return 0;
    }
    (usd_per_million * 1_000_000_000.0).round() as u64
}

fn scale_femto_usd(value: u64, ppm: u32) -> u64 {
    let scaled = u128::from(value) * u128::from(ppm) / u128::from(ONE_MILLION);
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        PriceTable, Rates, Usage, LIST_PRICES, MAX_USD_PER_MILLION, MIN_MULTIPLIER,
        MIN_USD_PER_MILLION, WEB_SEARCH_LIST_PRICE_FEMTO_USD,
    };
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
        assert_eq!(rates.input, 3_000_000_000);

        // Client-model literal match beats the canonical match.
        let rates = table
            .resolve("bedrock-eu", "sonnet-alias", "claude-sonnet-4-6-20260217")
            .expect("an override matches the client model");
        assert_eq!(rates.input, 2_000_000_000);

        // Neither literal matches: the dated snapshot is the same built-in.
        let rates = table
            .resolve("bedrock-eu", "unknown-alias", "claude-sonnet-4-6-20260217")
            .expect("an override matches the canonical built-in");
        assert_eq!(rates.input, 1_000_000_000);

        // A different upstream sees none of those rows, only the list price.
        let rates = table
            .resolve("bedrock-us", "sonnet-alias", "claude-sonnet-4-6")
            .expect("the list price prices a built-in");
        assert_eq!(rates.input, 3_000_000_000);
        assert_eq!(rates.output, 15_000_000_000);
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

    /// A row's `model` is stored normalized exactly as the lookup normalizes the
    /// request's: trimmed, stripped of Claude Code's `[1m]` hint, and compared
    /// case-insensitively. A row spelling the hint would otherwise never match,
    /// because the lookup strips it from the request.
    #[test]
    fn override_model_matching_normalizes_case_whitespace_and_the_hint() {
        let pricing = PricingConfig {
            multiplier: 1.0,
            overrides: vec![
                override_row("bedrock-eu", "Sonnet-Alias", 4.0),
                override_row("bedrock-eu", " custom[1m] ", 6.0),
            ],
        };
        let table = PriceTable::from_config(Some(&pricing));

        for model in ["custom", "custom[1m]", "CUSTOM[1M]"] {
            let rates = table
                .resolve("bedrock-eu", model, "unknown")
                .unwrap_or_else(|| panic!("the override matches {model}"));
            assert_eq!(rates.input, 6_000_000_000);
        }

        let rates = table
            .resolve("bedrock-eu", "SONNET-alias", "unknown")
            .expect("the override matches regardless of case");
        assert_eq!(rates.input, 4_000_000_000);

        let rates = table
            .resolve("bedrock-us", "haiku", "CLAUDE-Haiku-4-5")
            .expect("the list price matches regardless of case");
        assert_eq!(rates.input, 1_000_000_000);
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
        assert_eq!(list.input, 12_750_000_000); // 15 USD/M -> 15e9 femto -> x0.85
        assert_eq!(list.output, 63_750_000_000);

        let overridden = table
            .resolve("bedrock-eu", "claude-opus-4-1", "claude-opus-4-1")
            .expect("override");
        assert_eq!(overridden.input, 8_500_000_000); // 10 USD/M -> 10e9 femto -> x0.85

        assert_eq!(table.web_search_cost_femto_usd(), 8_500_000_000_000);

        let unscaled = PriceTable::from_config(None);
        assert_eq!(
            unscaled.web_search_cost_femto_usd(),
            WEB_SEARCH_LIST_PRICE_FEMTO_USD
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
        // (1000*3 + 200*15 + 10000*0.3 + 500*3.75) USD/M -> 10_875e9 femto-USD
        assert_eq!(rates.cost_femto_usd(&usage), 10_875_000_000_000);

        // Every class at its maximum: each product alone nearly fills `u128`,
        // so the four summed overflow it and must saturate rather than panic on
        // the debug build CI runs.
        let huge = Rates {
            input: u64::MAX,
            output: u64::MAX,
            cache_read: u64::MAX,
            cache_write: u64::MAX,
        };
        assert_eq!(
            huge.cost_femto_usd(&Usage {
                input_tokens: u64::MAX,
                output_tokens: u64::MAX,
                cache_read_input_tokens: u64::MAX,
                cache_creation_input_tokens: u64::MAX,
            }),
            u64::MAX
        );
    }

    /// The bounds `Config::validate_pricing` enforces are the reason money is
    /// carried in femto-USD: every rate inside them must quantize to a positive
    /// value that has not saturated. Under a coarser unit the smallest catalog
    /// rate at the smallest multiplier, and an override at the rate floor under
    /// a discount, both quantize to zero — a valid-looking config pricing every
    /// request at $0.
    #[test]
    fn the_configured_rate_bounds_neither_underflow_nor_saturate() {
        let table = PriceTable::from_config(Some(&PricingConfig {
            multiplier: MIN_MULTIPLIER,
            overrides: Vec::new(),
        }));
        for (id, ..) in LIST_PRICES {
            let rates = table
                .resolve("anthropic", id, id)
                .unwrap_or_else(|| panic!("{id} is a built-in"));
            let classes = [
                rates.input,
                rates.output,
                rates.cache_read,
                rates.cache_write,
            ];
            assert!(
                classes.iter().all(|rate| *rate > 0),
                "{id} prices at zero at the min multiplier: {rates:?}"
            );
        }

        // The ceiling is the last rate that still fits; one whole USD per
        // million more saturates instead of pricing what the config states.
        let at_ceiling = Rates::from_usd_per_million(MAX_USD_PER_MILLION, 1.0, 1.0, 1.0).input;
        assert!(
            at_ceiling < u64::MAX && at_ceiling > u64::MAX - 1_000_000_000,
            "{at_ceiling} must be the last rate below u64::MAX"
        );
        assert_eq!(
            Rates::from_usd_per_million(MAX_USD_PER_MILLION + 1.0, 1.0, 1.0, 1.0).input,
            u64::MAX
        );

        // The rate floor under a real discount, and the rate floor under the
        // multiplier floor — the latter is exactly 1 femto-USD per token, the
        // smallest nonzero rate, which is why these are the right floors.
        for (multiplier, expected) in [(0.85, 850_000), (MIN_MULTIPLIER, 1)] {
            let table = PriceTable::from_config(Some(&PricingConfig {
                multiplier,
                overrides: vec![override_row(
                    "bedrock-eu",
                    "vendor-alias",
                    MIN_USD_PER_MILLION,
                )],
            }));
            assert_eq!(
                table
                    .resolve("bedrock-eu", "vendor-alias", "vendor-alias")
                    .expect("the override prices the alias")
                    .input,
                expected,
                "multiplier {multiplier}"
            );
        }
    }
}
