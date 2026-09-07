//! Built-in USD list prices and the canonical model-id normalizer they are
//! keyed by.
//!
//! The catalog is the fallback the spend meter prices with when no
//! `[[server.spend.pricing.overrides]]` row matches. It holds Anthropic's
//! published list prices in USD per million tokens; a deployment whose real
//! price differs states that with an override row rather than by patching this
//! table.

/// `(id, input, output, cache_read, cache_write)` in USD per million tokens.
pub const LIST_PRICES: &[(&str, f64, f64, f64, f64)] = &[
    ("claude-fable-5-1", 10.0, 50.0, 0.25, 12.5),
    ("claude-mythos-5-1", 10.0, 50.0, 0.25, 12.5),
    ("claude-fable-5", 10.0, 50.0, 1.0, 12.5),
    ("claude-mythos-5", 10.0, 50.0, 1.0, 12.5),
    ("claude-opus-5", 5.0, 25.0, 0.5, 6.25),
    ("claude-opus-4-8", 5.0, 25.0, 0.5, 6.25),
    ("claude-opus-4-7", 5.0, 25.0, 0.5, 6.25),
    ("claude-opus-4-6", 5.0, 25.0, 0.5, 6.25),
    ("claude-sonnet-5", 2.0, 10.0, 0.2, 2.5),
    ("claude-sonnet-4-6", 3.0, 15.0, 0.3, 3.75),
    ("claude-haiku-4-5", 1.0, 5.0, 0.1, 1.25),
    ("claude-opus-4-5", 5.0, 25.0, 0.5, 6.25),
    ("claude-sonnet-4-5", 3.0, 15.0, 0.3, 3.75),
    ("claude-opus-4-1", 15.0, 75.0, 1.5, 18.75),
];

/// List price of one server-side web search, in nano-USD ($0.01 per request).
/// Per-request rather than per-token, so overrides never touch it.
pub const WEB_SEARCH_LIST_PRICE_NANO_USD: u64 = 10_000_000;

/// The built-in catalog id a provider-decorated model id refers to, or `None`
/// when the string is not a built-in at all.
///
/// Normalizes the three decorations shunt actually sees upstream: a Bedrock
/// region prefix and `anthropic.` namespace (`us.anthropic.claude-…`), a
/// Bedrock `-v<major>:<minor>` model-version suffix, and a dated snapshot
/// suffix in either the Anthropic (`-20260217`) or Vertex (`@20251101`) form.
/// It deliberately does not fuzzy-match: an operator alias like
/// `my-sonnet-alias` resolves to `None` so the caller can tell "unpriceable"
/// from "priced by guess".
pub fn canonical_builtin_id(model: &str) -> Option<&'static str> {
    let lowered = model.trim().to_ascii_lowercase();
    let mut rest = lowered.as_str();

    // `us.anthropic.`, `eu.anthropic.`, `global.anthropic.`, `anthropic.` —
    // every segment before `anthropic.` must be a bare alphabetic label.
    if let Some(index) = rest.find("anthropic.") {
        let prefix = &rest[..index];
        let region_prefix = prefix.is_empty()
            || (prefix.ends_with('.')
                && prefix.split_terminator('.').all(|segment| {
                    !segment.is_empty() && segment.chars().all(|c| c.is_ascii_alphabetic())
                }));
        if region_prefix {
            rest = &rest[index + "anthropic.".len()..];
        }
    }

    // Bedrock's `-v1:0` model version.
    if let Some((head, tail)) = rest.rsplit_once("-v") {
        if let Some((major, minor)) = tail.split_once(':') {
            if is_ascii_digits(major) && is_ascii_digits(minor) {
                rest = head;
            }
        }
    }

    // `-YYYYMMDD` / `@YYYYMMDD` snapshot.
    if let Some(index) = rest.rfind(['-', '@']) {
        let suffix = &rest[index + 1..];
        if suffix.len() == 8 && is_ascii_digits(suffix) {
            rest = &rest[..index];
        }
    }

    LIST_PRICES
        .iter()
        .find(|(id, ..)| *id == rest)
        .map(|(id, ..)| *id)
}

fn is_ascii_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::{canonical_builtin_id, LIST_PRICES};

    #[test]
    fn canonical_id_strips_provider_decorations_and_rejects_aliases() {
        assert_eq!(
            canonical_builtin_id("claude-sonnet-4-6-20260217"),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(
            canonical_builtin_id("us.anthropic.claude-sonnet-4-6-20260217-v1:0"),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(
            canonical_builtin_id("claude-opus-4-5@20251101"),
            Some("claude-opus-4-5")
        );
        assert_eq!(
            canonical_builtin_id("CLAUDE-Opus-4-1"),
            Some("claude-opus-4-1")
        );
        assert_eq!(canonical_builtin_id("my-sonnet-alias"), None);
        assert_eq!(canonical_builtin_id(""), None);
    }

    /// A duplicated id would make the first row silently shadow the second, and
    /// a non-positive rate would price requests at zero.
    #[test]
    fn list_prices_are_unique_and_positive() {
        let mut ids = LIST_PRICES.iter().map(|(id, ..)| *id).collect::<Vec<_>>();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate id in LIST_PRICES");

        for (id, input, output, cache_read, cache_write) in LIST_PRICES {
            for rate in [input, output, cache_read, cache_write] {
                assert!(rate.is_finite() && *rate > 0.0, "{id} has rate {rate}");
            }
        }
    }
}
