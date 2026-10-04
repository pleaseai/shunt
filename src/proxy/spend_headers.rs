//! `anthropic-ratelimit-unified-*` headers for a spend-capped principal.
//!
//! Values and selection follow the reference gateway (the track's
//! `reference-gateway.md`): a 2xx to a capped principal describes the
//! principal's own binding cap, the over-cap refusal carries the "exceeded"
//! set, and the fail-closed refusal carries only the `fetch_error` reason.
//! Claude Code reads these to show the 75% / 95% warnings and the limit
//! message, so an upstream's own pool-level values must never reach a capped
//! principal: every upstream `anthropic-ratelimit-*` header is stripped first.
//!
//! The headers are computed once, at admission, from the same snapshot the
//! pre-check decided on (as the reference does), and written onto the
//! response at `failover::forward`'s single exit — which every response path
//! of an admitted request returns through. That scope is admitted
//! `/v1/messages` requests only: `/v1/messages/count_tokens` is never assessed,
//! so its upstream `anthropic-ratelimit-*` headers pass through unchanged.

use axum::http::{HeaderMap, HeaderName, HeaderValue};

use crate::gateway::spend::meter::Binding;

const PREFIX: &str = "anthropic-ratelimit-";
const STATUS: HeaderName = HeaderName::from_static("anthropic-ratelimit-unified-status");
const RESET: HeaderName = HeaderName::from_static("anthropic-ratelimit-unified-reset");
const OVERAGE_RESET: HeaderName =
    HeaderName::from_static("anthropic-ratelimit-unified-overage-reset");
const OVERAGE_UTILIZATION: HeaderName =
    HeaderName::from_static("anthropic-ratelimit-unified-overage-utilization");
const SURPASSED_THRESHOLD: HeaderName =
    HeaderName::from_static("anthropic-ratelimit-unified-overage-surpassed-threshold");
const OVERAGE_PERIOD: HeaderName =
    HeaderName::from_static("anthropic-ratelimit-unified-overage-period");
const DISABLED_REASON: HeaderName =
    HeaderName::from_static("anthropic-ratelimit-unified-overage-disabled-reason");
const REPRESENTATIVE_CLAIM: HeaderName =
    HeaderName::from_static("anthropic-ratelimit-unified-representative-claim");
const OVERAGE_STATUS: HeaderName =
    HeaderName::from_static("anthropic-ratelimit-unified-overage-status");

/// What an admitted request's response does with the rate-limit family.
#[derive(Debug, Default)]
pub(crate) enum Plan {
    /// No capped principal: the response is left exactly as it is.
    #[default]
    Unchanged,
    /// A capped principal: strip every upstream `anthropic-ratelimit-*`
    /// header, then, on a 2xx, write these (none when the meter is
    /// unavailable and the request was forwarded fail-open).
    Replace(Vec<(HeaderName, HeaderValue)>),
}

impl Plan {
    /// Applies the plan to a response about to leave the gateway.
    pub(crate) fn apply(&self, status: axum::http::StatusCode, headers: &mut HeaderMap) {
        let Self::Replace(ours) = self else {
            return;
        };
        strip_upstream(headers);
        if status.is_success() {
            for (name, value) in ours {
                headers.insert(name.clone(), value.clone());
            }
        }
    }
}

/// Removes every `anthropic-ratelimit-*` field, every repetition of it
/// included (`HeaderMap::remove` drops all values of a name).
fn strip_upstream(headers: &mut HeaderMap) {
    let names: Vec<HeaderName> = headers
        .keys()
        .filter(|name| name.as_str().starts_with(PREFIX))
        .cloned()
        .collect();
    for name in names {
        headers.remove(name);
    }
}

/// The set a 2xx to a principal under its binding cap carries.
pub(crate) fn allowed(binding: &Binding) -> Vec<(HeaderName, HeaderValue)> {
    let threshold = binding.threshold();
    let status = if threshold.is_some() {
        "allowed_warning"
    } else {
        "allowed"
    };
    let mut headers = common(binding, status);
    headers.push((REPRESENTATIVE_CLAIM, HeaderValue::from_static("overage")));
    headers.push((OVERAGE_STATUS, HeaderValue::from_static(status)));
    headers
}

/// The set the over-cap refusal carries. No `representative-claim` or
/// `overage-status`: with those the client composes its own line and drops
/// the refusal's `error.message`.
pub(crate) fn exceeded(binding: &Binding, period: &'static str) -> Vec<(HeaderName, HeaderValue)> {
    let mut headers = common(binding, "rejected");
    headers.push((OVERAGE_PERIOD, HeaderValue::from_static(period)));
    headers.push((
        DISABLED_REASON,
        HeaderValue::from_static("org_spend_cap_reached"),
    ));
    headers
}

/// The one rate-limit header the fail-closed refusal carries.
pub(crate) fn fetch_error() -> (HeaderName, HeaderValue) {
    (DISABLED_REASON, HeaderValue::from_static("fetch_error"))
}

fn common(binding: &Binding, status: &'static str) -> Vec<(HeaderName, HeaderValue)> {
    let mut headers = vec![
        (STATUS, HeaderValue::from_static(status)),
        (RESET, HeaderValue::from(binding.reset_at)),
        (OVERAGE_RESET, HeaderValue::from(binding.reset_at)),
        (
            OVERAGE_UTILIZATION,
            number(hundredths(binding.utilization_hundredths())),
        ),
    ];
    if let Some(threshold) = binding.threshold() {
        headers.push((
            SURPASSED_THRESHOLD,
            number(hundredths(u128::from(threshold))),
        ));
    }
    headers
}

fn number(text: String) -> HeaderValue {
    HeaderValue::try_from(text).expect("a decimal number is a valid header value")
}

/// A value in hundredths as JavaScript's `String(n / 100)` prints it: no
/// trailing zeros, no trailing point (`82` → `0.82`, `50` → `0.5`, `100` → `1`).
fn hundredths(value: u128) -> String {
    let (whole, fraction) = (value / 100, value % 100);
    match fraction {
        0 => whole.to_string(),
        _ if fraction % 10 == 0 => format!("{whole}.{}", fraction / 10),
        _ => format!("{whole}.{fraction:02}"),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::*;
    use crate::gateway::spend::store::Period;

    const CENT: u128 = 10_000_000_000_000;

    fn binding(spent_cents: u128, cap_cents: u128) -> Binding {
        Binding {
            period: Period::Weekly,
            spent: spent_cents * CENT,
            cap: cap_cents * CENT,
            reset_at: 1_791_590_400,
        }
    }

    fn rendered(headers: &[(HeaderName, HeaderValue)]) -> Vec<(String, String)> {
        headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_str().unwrap().to_string()))
            .collect()
    }

    fn pair(name: &str, value: &str) -> (String, String) {
        (name.to_string(), value.to_string())
    }

    #[test]
    fn numbers_print_like_javascript() {
        let cases = [
            (82, "0.82"),
            (50, "0.5"),
            (0, "0"),
            (100, "1"),
            (120, "1.2"),
            (105, "1.05"),
            (7, "0.07"),
            (95, "0.95"),
            (75, "0.75"),
        ];
        for (value, text) in cases {
            assert_eq!(hundredths(value), text, "{value}");
        }
    }

    #[test]
    fn a_warning_carries_the_threshold_and_the_overage_claim() {
        assert_eq!(
            rendered(&allowed(&binding(82, 100))),
            vec![
                pair("anthropic-ratelimit-unified-status", "allowed_warning"),
                pair("anthropic-ratelimit-unified-reset", "1791590400"),
                pair("anthropic-ratelimit-unified-overage-reset", "1791590400"),
                pair("anthropic-ratelimit-unified-overage-utilization", "0.82"),
                pair(
                    "anthropic-ratelimit-unified-overage-surpassed-threshold",
                    "0.75"
                ),
                pair(
                    "anthropic-ratelimit-unified-representative-claim",
                    "overage"
                ),
                pair(
                    "anthropic-ratelimit-unified-overage-status",
                    "allowed_warning"
                ),
            ]
        );
    }

    #[test]
    fn below_the_first_threshold_is_plain_allowed() {
        let headers = rendered(&allowed(&binding(75, 100)));
        assert!(headers.contains(&pair("anthropic-ratelimit-unified-status", "allowed")));
        assert!(headers.contains(&pair(
            "anthropic-ratelimit-unified-overage-utilization",
            "0.75"
        )));
        assert!(headers.iter().all(|(name, _)| !name.ends_with("threshold")));
    }

    #[test]
    fn the_exceeded_set_names_the_period_and_omits_the_claim() {
        assert_eq!(
            rendered(&exceeded(&binding(120, 100), "weekly")),
            vec![
                pair("anthropic-ratelimit-unified-status", "rejected"),
                pair("anthropic-ratelimit-unified-reset", "1791590400"),
                pair("anthropic-ratelimit-unified-overage-reset", "1791590400"),
                pair("anthropic-ratelimit-unified-overage-utilization", "1.2"),
                pair(
                    "anthropic-ratelimit-unified-overage-surpassed-threshold",
                    "1"
                ),
                pair("anthropic-ratelimit-unified-overage-period", "weekly"),
                pair(
                    "anthropic-ratelimit-unified-overage-disabled-reason",
                    "org_spend_cap_reached"
                ),
            ]
        );
    }

    #[test]
    fn replace_strips_every_upstream_value_and_writes_only_on_success() {
        let plan = Plan::Replace(allowed(&binding(10, 100)));
        let upstream = || {
            let mut headers = HeaderMap::new();
            headers.append(STATUS, HeaderValue::from_static("rejected"));
            headers.append(STATUS, HeaderValue::from_static("rejected"));
            headers.append(
                "anthropic-ratelimit-tokens-limit",
                HeaderValue::from_static("1"),
            );
            headers.append("x-other", HeaderValue::from_static("kept"));
            headers
        };

        let mut ok = upstream();
        plan.apply(StatusCode::OK, &mut ok);
        assert_eq!(ok.get_all(STATUS).iter().count(), 1);
        assert_eq!(ok[STATUS], "allowed");
        assert!(!ok.contains_key("anthropic-ratelimit-tokens-limit"));
        assert_eq!(ok["x-other"], "kept");

        let mut failed = upstream();
        plan.apply(StatusCode::BAD_GATEWAY, &mut failed);
        assert!(failed.keys().all(|name| !name.as_str().starts_with(PREFIX)));
        assert_eq!(failed["x-other"], "kept");

        let mut untouched = upstream();
        Plan::Unchanged.apply(StatusCode::OK, &mut untouched);
        assert_eq!(untouched.get_all(STATUS).iter().count(), 2);
    }
}
