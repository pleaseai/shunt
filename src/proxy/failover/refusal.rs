//! The account-level refusal marker an adapter attaches to a relayed response
//! (issue #817).
//!
//! The ordered chain decides from the status alone, and a 400 or 402 is
//! terminal there: resending a malformed request to every upstream is the
//! reason the rule exists. Some 400s and 402s are not about the request,
//! though — the upstream refused the whole account (a spend limit, an empty
//! credit balance, a billing error), and the next upstream may well serve the
//! same request. Only the adapter knows its protocol's error envelope, so it
//! judges the body and leaves this marker in the response extensions; the
//! chain reads the marker and advances without knowing any provider's shape.
//! The response itself keeps the upstream's status and body intact, so a
//! chain that exhausts on refusals relays the upstream's own answer.
//!
//! A refusal also starts a skip window for the refusing `api_key` upstream
//! (`crate::accounts::UpstreamSkips`), so later requests go past it without a
//! refused round trip until the window ends.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;

use crate::{accounts::UpstreamSkips, routing::Route, server::AppState};

/// An upstream refused the account behind this response, not the request.
/// The adapter that judged the body has already recorded the refusal through
/// [`record_refusal`]; the chain only reads that it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AccountRefused;

/// Record an account-level refusal where the adapter classified it: start the
/// upstream's skip window (unless `skips` is `None`, a passthrough route that
/// lends the caller's own credential and is never skipped), then log the
/// refusal exactly once. A named function for the same reason as
/// `log_error_advance`: the adapters run inside spawned tasks a test
/// subscriber never reaches.
pub(crate) fn record_refusal(
    skips: Option<&UpstreamSkips>,
    provider: &str,
    model: &str,
    status: StatusCode,
    kind: &str,
    skip_for: Duration,
) {
    let skip_until = skips.map(|skips| {
        let lasts = skips.record(provider, skip_for);
        let until = SystemTime::now() + lasts;
        until.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
    });
    match skip_until {
        Some(skip_until) => tracing::warn!(
            provider = %provider,
            model = %model,
            status = status.as_u16(),
            refusal = kind,
            skip_until_epoch_secs = skip_until,
            "upstream refused the account; skipping it until the window ends"
        ),
        None => tracing::warn!(
            provider = %provider,
            model = %model,
            status = status.as_u16(),
            refusal = kind,
            "upstream refused the account; passthrough upstream is not skipped"
        ),
    }
}

/// Drop the routes whose upstream is inside a refusal skip window, keeping
/// the order of the rest. A route that is passthrough now is never dropped,
/// even when a window recorded earlier under its name survives a reload that
/// switched the upstream's auth. When every route is skipped the configured
/// chain is returned unchanged, so the request is still tried in order. Shared by every
/// entry that builds a chain: `forward`, and `routing::serve`'s judge and
/// gated-capture calls, which resolve their own chains.
pub(crate) fn skip_refused_routes(state: &AppState, routes: Vec<Route>) -> Vec<Route> {
    state
        .accounts
        .upstream_skips()
        .retain_unskipped(routes, |route| {
            (!state.config.route_is_passthrough(route)).then_some(route.provider.as_str())
        })
}

/// [`skip_refused_routes`] for a caller that dispatches the chain with the
/// caller's own headers: when skipping drops the configured primary, the
/// caller's credential slots are removed from `headers` in the same step.
/// The credential-retention rule keys on the primary: a passthrough fallback
/// behind a credential-injecting primary is off-origin and has both slots
/// stripped, so a skip must not promote it to primary and let it keep the
/// caller's `authorization` / `x-api-key`. Every chain-building entry that
/// forwards caller headers uses this, so the skip and the strip cannot be
/// applied apart.
pub(crate) fn skip_refused_routes_for_caller(
    state: &AppState,
    routes: Vec<Route>,
    headers: &mut axum::http::HeaderMap,
) -> Vec<Route> {
    let configured_primary = routes.first().cloned();
    let routes = skip_refused_routes(state, routes);
    if routes.first() != configured_primary.as_ref() {
        headers.remove("authorization");
        headers.remove("x-api-key");
    }
    routes
}
