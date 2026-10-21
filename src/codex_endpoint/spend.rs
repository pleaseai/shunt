//! Spend-limit admission for the inbound Codex endpoint (`[server.spend]`).
//!
//! The gate resolves the principal, asks the spend store for its
//! [`Assessment`](crate::gateway::spend::meter::Assessment), and either admits
//! the turn or returns a [`Refusal`]. A refusal is rendered here, in the OpenAI
//! `usage_limit_reached` shape the Codex CLI recognises: it must not travel
//! through [`crate::error::into_openai_error_shape`], which keeps only the
//! message, type and `Retry-After` and would drop `code` and `resets_at`.
//!
//! The decision and the wording are the `/v1/messages` gate's
//! ([`crate::proxy::spend_gate`]); only the envelope differs. [`Refusal`] holds
//! the parts separately so a WebSocket turn can send the same error object as a
//! frame.

use axum::{
    http::{header::RETRY_AFTER, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{json, Value};

use crate::{
    gateway::spend::meter::{now_secs, Check},
    proxy::spend_gate::{period_name, principal_for, reached_message, with_blocked_message},
    server::AppState,
};

const SHOULD_RETRY: HeaderName = HeaderName::from_static("x-should-retry");
const PROMO_MESSAGE: HeaderName = HeaderName::from_static("x-codex-promo-message");
/// Both the error `type` and `code` the Codex CLI keys on.
const USAGE_LIMIT_REACHED: &str = "usage_limit_reached";

/// The turn may proceed to an upstream.
#[derive(Debug)]
pub(super) struct Admitted;

/// A turn the gate refused, as the parts every transport renders.
#[derive(Debug)]
pub(super) struct Refusal {
    message: String,
    /// The binding cap's reset (epoch seconds); `None` for a fail-closed
    /// refusal, which must not invite a retry.
    resets_at: Option<u64>,
    retry_after: Option<u64>,
    /// `blocked_message` when it is a valid header value.
    promo: Option<HeaderValue>,
}

impl Refusal {
    /// The OpenAI `error` object.
    pub(super) fn error(&self) -> Value {
        let mut error = json!({
            "message": self.message,
            "type": USAGE_LIMIT_REACHED,
            "code": USAGE_LIMIT_REACHED,
        });
        if let Some(resets_at) = self.resets_at {
            error["resets_at"] = json!(resets_at);
        }
        error
    }

    /// The headers a refusal carries.
    pub(super) fn headers(&self) -> Vec<(HeaderName, HeaderValue)> {
        let mut headers = vec![(SHOULD_RETRY, HeaderValue::from_static("false"))];
        if let Some(seconds) = self.retry_after {
            headers.push((RETRY_AFTER, HeaderValue::from(seconds)));
        }
        if let Some(promo) = &self.promo {
            headers.push((PROMO_MESSAGE, promo.clone()));
        }
        headers
    }

    pub(super) fn into_response(self) -> Response {
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(json!({ "error": self.error() })),
        )
            .into_response();
        for (name, value) in self.headers() {
            response.headers_mut().insert(name, value);
        }
        response
    }
}

/// Admits or refuses one metered Codex turn for `client` (the authenticated
/// caller, `None` when `[server.auth]` is off). Always admits without
/// `[server.spend]`. Every turn is assessed afresh, so a refused principal
/// recovers when its period resets.
pub(super) fn admit(state: &AppState, client: Option<&str>) -> Result<Admitted, Refusal> {
    let Some(spend) = state.config.server.spend.as_ref() else {
        return Ok(Admitted);
    };
    // Every route on this endpoint reaches a gateway-credentialed upstream.
    let Some(principal) = principal_for(client, true) else {
        return Ok(Admitted);
    };
    let blocked_message = spend.blocked_message.as_deref();
    let promo = blocked_message.and_then(promo_header);
    let now = now_secs();
    let assessment = state.gateway_stores.spend.assess(&principal, now);
    match (assessment.check, assessment.binding) {
        (Check::Blocked { period, reset_at }, _) => {
            tracing::info!(
                principal,
                period = period_name(period),
                "spend limit reached"
            );
            Err(Refusal {
                message: reached_message(period, reset_at, blocked_message),
                resets_at: Some(reset_at),
                retry_after: Some(reset_at.saturating_sub(now).max(1)),
                promo,
            })
        }
        // As on `/v1/messages`: only a principal with a cap is refused.
        (Check::Unavailable, Some(_)) if spend.enforcement.fail_closed_on_error => {
            tracing::warn!(
                principal,
                "spend state unavailable; refusing (fail_closed_on_error)"
            );
            Err(Refusal {
                message: with_blocked_message(
                    "spend limit unavailable".to_string(),
                    blocked_message,
                ),
                resets_at: None,
                retry_after: None,
                promo,
            })
        }
        (Check::Unavailable, _) => {
            tracing::warn!(principal, "spend state unavailable; forwarding the request");
            Ok(Admitted)
        }
        (Check::Allow, _) => Ok(Admitted),
    }
}

/// `blocked_message` as an `x-codex-promo-message` value, or `None` when it is
/// not plain visible ASCII. `HeaderValue::from_str` alone also admits obs-text
/// (bytes `0x80..`), which a client reading the header with `to_str()` cannot
/// decode, so non-ASCII text rides in the message only.
fn promo_header(text: &str) -> Option<HeaderValue> {
    if !text.is_ascii() {
        return None;
    }
    HeaderValue::from_str(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(promo: Option<&str>) -> Refusal {
        Refusal {
            message: "spend limit reached (daily; resets 2026-11-01 00:00 UTC)".to_string(),
            resets_at: Some(1_793_491_200),
            retry_after: Some(42),
            promo: promo.and_then(promo_header),
        }
    }

    #[test]
    fn only_visible_ascii_becomes_a_promo_header() {
        assert_eq!(promo_header("Ask FinOps.").unwrap(), "Ask FinOps.");
        assert!(promo_header("bad\nvalue").is_none());
        // `HeaderValue::from_str` accepts this; the promo header must not.
        assert!(HeaderValue::from_str("Finanzabteilung \u{2014} bitte").is_ok());
        assert!(promo_header("Finanzabteilung \u{2014} bitte").is_none());
    }

    #[test]
    fn the_error_object_carries_type_code_and_resets_at() {
        let error = refusal(None).error();
        assert_eq!(error["type"], "usage_limit_reached");
        assert_eq!(error["code"], "usage_limit_reached");
        assert_eq!(error["resets_at"], 1_793_491_200_u64);
        assert!(error["message"]
            .as_str()
            .unwrap()
            .starts_with("spend limit reached"));
    }

    #[test]
    fn a_fail_closed_refusal_has_no_reset_and_no_retry_after() {
        let refusal = Refusal {
            message: "spend limit unavailable".to_string(),
            resets_at: None,
            retry_after: None,
            promo: None,
        };
        assert!(refusal.error().get("resets_at").is_none());
        let names: Vec<_> = refusal
            .headers()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, vec![SHOULD_RETRY]);
    }

    #[test]
    fn the_promo_header_rides_only_when_the_text_is_a_valid_header_value() {
        let with = refusal(Some("Ask FinOps."));
        assert!(with
            .headers()
            .iter()
            .any(|(name, value)| name == PROMO_MESSAGE && value == "Ask FinOps."));
        assert!(!refusal(Some("bad\nvalue"))
            .headers()
            .iter()
            .any(|(name, _)| name == PROMO_MESSAGE));
    }

    #[test]
    fn the_response_is_a_429_with_headers() {
        let response = refusal(Some("Ask FinOps.")).into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[RETRY_AFTER], "42");
        assert_eq!(response.headers()[SHOULD_RETRY], "false");
    }
}
