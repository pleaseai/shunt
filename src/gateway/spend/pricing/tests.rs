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
    // Parts per billion: 850_000_000 ppb is a 0.85 multiplier.
    assert_eq!(rates.scaled(850_000_000).input, 2); // 2.55 -> 2
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

/// An accepted multiplier is applied to the nearest part per billion.
/// `validate_pricing` takes any finite value in `[MIN_MULTIPLIER, 1.0]`, and
/// no fixed-point scale represents every such `f64` exactly — the guarantee
/// is the quantization bound, not exactness. What must not happen is the
/// per-million behavior this replaced, where a value the validator accepted
/// moved to a *materially* different discount.
#[test]
fn an_accepted_multiplier_is_applied_to_the_nearest_part_per_billion() {
    // Both spellings round to the *same* parts-per-million bucket as a
    // neighbouring value, which is what made the coarser store wrong:
    // `0.0000014` collapsed onto the `0.000001` floor (a 29% undercharge)
    // and `0.9999996` collapsed onto an undiscounted `1.0`.
    for (multiplier, rate_usd_per_million, expected) in [
        // 1 USD/M = 1e9 femto-USD/token, so the femto rate reads as the
        // multiplier itself shifted nine places.
        (0.0000014_f64, 1.0_f64, 1_400_u64),
        (0.9999996, 1.0, 999_999_600),
        // The floor still lands on exactly 1 femto-USD per token.
        (MIN_MULTIPLIER, MIN_USD_PER_MILLION, 1),
    ] {
        let table = PriceTable::from_config(Some(&PricingConfig {
            multiplier,
            overrides: vec![override_row(
                "bedrock-eu",
                "vendor-alias",
                rate_usd_per_million,
            )],
        }));
        assert_eq!(
            table
                .resolve("bedrock-eu", "vendor-alias", "vendor-alias")
                .expect("the override prices the alias")
                .input,
            expected,
            "multiplier {multiplier} at {rate_usd_per_million} USD/M"
        );
    }

    // The residual is a rounding bound, not exactness: a multiplier between
    // two parts per billion moves to the nearer one. The error that leaves
    // is at most 5e-10 absolute, which is worst relative to the smallest
    // accepted multiplier — 0.05% at `MIN_MULTIPLIER`, and less above it.
    for (multiplier, nearest_ppb) in [
        (0.000_001_000_4_f64, 1_000_u64),
        (0.999_999_999_6, 1_000_000_000),
    ] {
        let applied = PriceTable::from_config(Some(&PricingConfig {
            multiplier,
            overrides: vec![override_row("bedrock-eu", "vendor-alias", 1.0)],
        }))
        .resolve("bedrock-eu", "vendor-alias", "vendor-alias")
        .expect("the override prices the alias")
        .input;
        assert_eq!(applied, nearest_ppb, "multiplier {multiplier}");

        let configured = multiplier * 1e9;
        assert!(
            (applied as f64 - configured).abs() <= 0.5,
            "multiplier {multiplier} moved more than half a part per billion"
        );
    }
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
