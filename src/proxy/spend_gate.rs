//! `/v1/messages` spend-limit admission (`[server.spend]`).
//!
//! The check is an in-memory lookup against the stage-1 caps and the meter's
//! counters ([`SpendStore::assess`](crate::gateway::spend::SpendStore::assess)):
//! no I/O, and no clone of the limit tables. It runs before the first
//! upstream-capable step of a request, so a refused principal reaches no
//! upstream, router judge and classifier calls included.

use axum::{
    http::{header::RETRY_AFTER, HeaderName, HeaderValue, StatusCode},
    response::IntoResponse,
};

use super::{spend_headers, spend_headers::Plan, ForwardError};
use crate::{
    error::ShuntError,
    gateway::spend::{
        meter::{now_secs, reset_label, Check, ANONYMOUS_PRINCIPAL},
        store::Period,
    },
    routing::{AdapterKind, Route},
    server::AppState,
};

const SHOULD_RETRY: HeaderName = HeaderName::from_static("x-should-retry");

/// Whether a route's turns are metered: it reaches an upstream (not `noop`)
/// and injects a gateway credential (not a passthrough route, whose caller
/// pays). The one definition shared by admission and by [`ServedTarget`].
pub(crate) fn route_meters(state: &AppState, route: &Route) -> bool {
    route.adapter != AdapterKind::Noop && !state.config.route_is_passthrough(route)
}

/// What the pricing table keys on for the route that served a turn: the
/// upstream, the client-facing model, and the model string sent upstream. The
/// only place the classifier-pinned model and the "route meters spend"
/// predicate are applied.
#[derive(Clone, Debug)]
pub(crate) struct ServedTarget {
    pub provider: String,
    pub model: String,
    /// The model string the upstream was sent (classifier pin included).
    pub upstream_model: String,
    /// Whether the route is metered at all ([`route_meters`]).
    pub meters: bool,
}

impl ServedTarget {
    pub(crate) fn of(state: &AppState, route: &Route, body: &serde_json::Value) -> Self {
        Self {
            provider: route.provider.clone(),
            model: route.model.clone(),
            upstream_model: crate::adapters::anthropic::effective_upstream_model(
                &state.config,
                route,
                body,
            ),
            meters: route_meters(state, route),
        }
    }
}

/// The principal a request's spend is attributed to.
///
/// `None` means unmetered: no route in the chain meters spend
/// ([`route_meters`]) — it forwards the caller's own upstream credential, so
/// the caller pays, or it is `noop` — and there is nothing to cap. A metered
/// chain with no authenticated identity shares the one anonymous principal.
pub(crate) fn principal_for(client: Option<&str>, meters_spend: bool) -> Option<String> {
    if !meters_spend {
        return None;
    }
    Some(client.map_or_else(|| ANONYMOUS_PRINCIPAL.to_string(), ToOwned::to_owned))
}

/// Refuses `principal` when it has reached a cap, and otherwise returns what
/// the response must do with its `anthropic-ratelimit-*` headers. A no-op
/// ([`Plan::Unchanged`]) without `[server.spend]`, for an unmetered request,
/// for `count_tokens` (never refused), and for a principal with no cap.
pub(crate) fn enforce(
    state: &AppState,
    principal: Option<&str>,
    count_tokens: bool,
) -> Result<Plan, Box<ForwardError>> {
    let (Some(spend), Some(principal)) = (state.config.server.spend.as_ref(), principal) else {
        return Ok(Plan::Unchanged);
    };
    if count_tokens {
        return Ok(Plan::Unchanged);
    }
    let now = now_secs();
    let assessment = state.gateway_stores.spend.assess(principal, now);
    match (assessment.check, assessment.binding) {
        (Check::Allow, None) => Ok(Plan::Unchanged),
        (Check::Allow, Some(binding)) => Ok(Plan::Replace(spend_headers::allowed(&binding))),
        (Check::Blocked { period, reset_at }, binding) => {
            tracing::info!(
                principal,
                period = period_name(period),
                "spend limit reached"
            );
            let message = reached_message(period, reset_at, spend.blocked_message.as_deref());
            let retry_after = reset_at.saturating_sub(now).max(1);
            // `Blocked` is derived from the binding cap, so it is always set.
            let headers = binding
                .map(|binding| spend_headers::exceeded(&binding))
                .unwrap_or_default();
            Err(refusal(message, Some(retry_after), headers))
        }
        // Fail-closed refuses only a principal with a cap to enforce; with no
        // cap in any period (`binding` is `None`) there is nothing to protect,
        // so it falls through to the forward arm below.
        (Check::Unavailable, Some(_)) if spend.enforcement.fail_closed_on_error => {
            tracing::warn!(
                principal,
                "spend state unavailable; refusing (fail_closed_on_error)"
            );
            Err(refusal(
                with_blocked_message(
                    "spend limit unavailable".to_string(),
                    spend.blocked_message.as_deref(),
                ),
                None,
                vec![spend_headers::fetch_error()],
            ))
        }
        (Check::Unavailable, binding) => {
            tracing::warn!(principal, "spend state unavailable; forwarding the request");
            // Fail-open carries no rate-limit headers; a capped principal
            // still never sees the upstream's own.
            Ok(binding.map_or(Plan::Unchanged, |_| Plan::Replace(Vec::new())))
        }
    }
}

/// The over-cap refusal's message, naming the binding cap's period and reset.
fn reached_message(period: Period, reset_at: u64, blocked_message: Option<&str>) -> String {
    with_blocked_message(
        format!(
            "spend limit reached ({}; resets {})",
            period_name(period),
            reset_label(reset_at)
        ),
        blocked_message,
    )
}

/// Appends the operator's `blocked_message` after an em dash, as the reference
/// gateway does, on both refusal messages.
fn with_blocked_message(base: String, blocked_message: Option<&str>) -> String {
    match blocked_message {
        Some(extra) => format!("{base} — {extra}"),
        None => base,
    }
}

pub(super) fn period_name(period: Period) -> &'static str {
    match period {
        Period::Daily => "daily",
        Period::Weekly => "weekly",
        Period::Monthly => "monthly",
    }
}

fn refusal(
    message: String,
    retry_after: Option<u64>,
    rate_limit: Vec<(HeaderName, HeaderValue)>,
) -> Box<ForwardError> {
    let mut response = ShuntError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "billing_error",
        message.clone(),
    )
    .into_response();
    let headers = response.headers_mut();
    if let Some(seconds) = retry_after {
        headers.insert(RETRY_AFTER, HeaderValue::from(seconds));
    }
    headers.insert(SHOULD_RETRY, HeaderValue::from_static("false"));
    for (name, value) in rate_limit {
        headers.insert(name, value);
    }
    Box::new(ForwardError::new(message, Box::new(response)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::spend::{
        meter::{window, SpendMeter, FEMTO_USD_PER_CENT},
        store::{Scope, SpendLimit},
    };

    fn cap(period: Period) -> SpendLimit {
        SpendLimit {
            id: "spl_t".into(),
            amount: Some("1".into()),
            created_at: String::new(),
            currency: "USD".into(),
            period,
            scope: Scope::User {
                user_id: "a".into(),
            },
            object_type: "spend_limit".into(),
            updated_at: String::new(),
        }
    }

    /// 2026-10-31 (a Saturday) 12:00 UTC: the daily and monthly windows both
    /// reset at 2026-11-01 00:00, the weekly one on Monday.
    const LAST_DAY: u64 = 1_793_448_000;

    #[test]
    fn an_equal_reset_names_the_later_period_in_message_and_headers() {
        assert_eq!(
            window(Period::Daily, LAST_DAY).end,
            window(Period::Monthly, LAST_DAY).end
        );
        let limits = [cap(Period::Daily), cap(Period::Monthly)];
        let meter = SpendMeter::default();
        meter.record("a", LAST_DAY, FEMTO_USD_PER_CENT as u64);

        let assessment = meter.assess(&limits, "a", LAST_DAY);
        let Check::Blocked { period, reset_at } = assessment.check else {
            panic!("both caps are reached: {:?}", assessment.check);
        };
        assert_eq!(period, Period::Monthly);
        assert_eq!(
            assessment.binding.map(|binding| binding.period),
            Some(period)
        );
        assert!(
            reached_message(period, reset_at, None)
                .starts_with("spend limit reached (monthly; resets 2026-11-01 00:00"),
            "{}",
            reached_message(period, reset_at, None)
        );
    }
}
