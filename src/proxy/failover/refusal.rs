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

use std::time::Duration;

use axum::http::StatusCode;

/// An upstream refused the account behind this response, not the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AccountRefused {
    /// Stable snake_case label naming the refusal (`spend_limit`,
    /// `credit_balance`, `billing`), for logs and metrics.
    pub(crate) kind: &'static str,
    /// How long the refusing credential is not worth retrying, measured from
    /// the moment the refusal was classified.
    // Read by the per-upstream skip state (T004 of #817).
    #[allow(dead_code)]
    pub(crate) skip_for: Duration,
}

/// Record a chain advance caused by an account-level refusal. A named function
/// for the same reason as `log_error_advance`: the chain runs inside spawned
/// tasks a test subscriber never reaches.
pub(crate) fn log_refusal_advance(provider: &str, model: &str, status: StatusCode, kind: &str) {
    tracing::warn!(
        provider = %provider,
        model = %model,
        status = status.as_u16(),
        refusal = kind,
        "upstream refused the account; failover advancing"
    );
}
