//! `/v1/messages` spend-limit admission (`[server.spend]`).
//!
//! The check is an in-memory lookup against the stage-1 caps and the meter's
//! counters ([`SpendStore::check`](crate::gateway::spend::SpendStore::check)):
//! no I/O, and no clone of the limit tables. It runs before the first
//! upstream-capable step of a request, so a refused principal reaches no
//! upstream, router judge and classifier calls included.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    http::{header::RETRY_AFTER, HeaderName, HeaderValue, StatusCode},
    response::IntoResponse,
};

use super::ForwardError;
use crate::{
    error::ShuntError,
    gateway::spend::{
        meter::{reset_label, Check, ANONYMOUS_PRINCIPAL},
        store::Period,
    },
    server::AppState,
};

const SHOULD_RETRY: HeaderName = HeaderName::from_static("x-should-retry");

/// The principal a request's spend is attributed to.
///
/// `None` means unmetered: the chain forwards the caller's own upstream
/// credential, so the caller pays and there is nothing to cap. A
/// credential-injecting chain with no authenticated identity shares the one
/// anonymous principal.
pub(crate) fn principal_for(client: Option<&str>, injects_credential: bool) -> Option<String> {
    if !injects_credential {
        return None;
    }
    Some(client.map_or_else(|| ANONYMOUS_PRINCIPAL.to_string(), ToOwned::to_owned))
}

/// Refuses `principal` when it has reached a cap. A no-op without
/// `[server.spend]`, for an unmetered request, and for `count_tokens`, which
/// is never refused.
pub(crate) fn enforce(
    state: &AppState,
    principal: Option<&str>,
    count_tokens: bool,
) -> Result<(), Box<ForwardError>> {
    let (Some(spend), Some(principal)) = (state.config.server.spend.as_ref(), principal) else {
        return Ok(());
    };
    if count_tokens {
        return Ok(());
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    match state.gateway_stores.spend.check(principal, now) {
        Check::Allow => Ok(()),
        Check::Blocked { period, reset_at } => {
            tracing::info!(
                principal,
                period = period_name(period),
                "spend limit reached"
            );
            let message = with_blocked_message(
                format!(
                    "spend limit reached ({}; resets {})",
                    period_name(period),
                    reset_label(reset_at)
                ),
                spend.blocked_message.as_deref(),
            );
            let retry_after = reset_at.saturating_sub(now).max(1);
            Err(refusal(message, Some(retry_after)))
        }
        Check::Unavailable if spend.enforcement.fail_closed_on_error => {
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
            ))
        }
        Check::Unavailable => {
            tracing::warn!(principal, "spend state unavailable; forwarding the request");
            Ok(())
        }
    }
}

/// Appends the operator's `blocked_message` after an em dash, as the reference
/// gateway does, on both refusal messages.
fn with_blocked_message(base: String, blocked_message: Option<&str>) -> String {
    match blocked_message {
        Some(extra) => format!("{base} — {extra}"),
        None => base,
    }
}

fn period_name(period: Period) -> &'static str {
    match period {
        Period::Daily => "daily",
        Period::Weekly => "weekly",
        Period::Monthly => "monthly",
    }
}

fn refusal(message: String, retry_after: Option<u64>) -> Box<ForwardError> {
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
    Box::new(ForwardError {
        message,
        response: Box::new(response),
    })
}
