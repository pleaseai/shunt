//!
//! Three upstream answers mean the whole account (not the request) is out of
//! room: the organisation or workspace spend limit, an exhausted credit
//! balance, and any `billing_error`. Retrying the same account is pointless
//! until the cause clears, so the caller skips it for a window. The spend-limit
//! message states when access returns; the other two carry no time.
//!
//! Matching is on the start of the documented message, never a substring: a
//! validation 400 that merely mentions `usage limits` must stay terminal.

// T002 wires this module up and removes the allow.
#![cfg_attr(not(test), allow(dead_code))]

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::auth::claude::usage::parse_rfc3339_to_epoch_secs;

/// Skip window when the refusal states no usable resume time.
pub(crate) const REFUSAL_DEFAULT_SKIP: Duration = Duration::from_secs(60 * 60);

const SPEND_LIMIT_PREFIXES: [&str; 2] = [
    "You have reached your specified API usage limits",
    "You have reached your specified workspace API usage limits",
];
const CREDIT_BALANCE_PREFIX: &str = "Your credit balance is too low to access the Anthropic API";
const RESUME_MARKER: &str = "regain access on ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefusalKind {
    SpendLimit,
    CreditBalance,
    Billing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AccountRefusal {
    pub(crate) kind: RefusalKind,
    /// Stated access-resume instant, seconds since the Unix epoch (spend limit only).
    pub(crate) resume_at_epoch_secs: Option<u64>,
}

impl AccountRefusal {
    /// How long to skip the account: until the stated resume time when it is
    /// still ahead of `now`, otherwise [`REFUSAL_DEFAULT_SKIP`].
    pub(crate) fn skip_window(&self, now: SystemTime) -> Duration {
        let now_secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        match self.resume_at_epoch_secs {
            Some(resume) if resume > now_secs => Duration::from_secs(resume - now_secs),
            _ => REFUSAL_DEFAULT_SKIP,
        }
    }
}

/// Classify an upstream error response; `None` means it is not an account-level refusal.
pub(crate) fn classify(status: u16, body: &[u8]) -> Option<AccountRefusal> {
    if status != 400 && status != 402 {
        return None;
    }
    let value: Value = serde_json::from_slice(body).ok()?;
    let error = value.get("error")?;
    let message = error.get("message").and_then(Value::as_str).unwrap_or("");
    let error_type = error.get("type").and_then(Value::as_str);

    if status == 402 && error_type == Some("billing_error") {
        return Some(AccountRefusal {
            kind: RefusalKind::Billing,
            resume_at_epoch_secs: None,
        });
    }
    if message.starts_with(CREDIT_BALANCE_PREFIX) {
        return Some(AccountRefusal {
            kind: RefusalKind::CreditBalance,
            resume_at_epoch_secs: None,
        });
    }
    if status == 400 && SPEND_LIMIT_PREFIXES.iter().any(|p| message.starts_with(p)) {
        return Some(AccountRefusal {
            kind: RefusalKind::SpendLimit,
            resume_at_epoch_secs: parse_resume(message),
        });
    }
    None
}

/// Parse `YYYY-MM-DD at HH:MM UTC` following `regain access on `.
fn parse_resume(message: &str) -> Option<u64> {
    let rest = &message[message.find(RESUME_MARKER)? + RESUME_MARKER.len()..];
    let (date, rest) = rest.split_once(" at ")?;
    let (clock, _) = rest.split_once(" UTC")?;
    let well_formed = date.len() == 10 && clock.len() == 5;
    well_formed
        .then(|| parse_rfc3339_to_epoch_secs(&format!("{date}T{clock}:00Z")))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(error_type: &str, message: &str) -> Vec<u8> {
        serde_json::json!({"type": "error", "error": {"type": error_type, "message": message}})
            .to_string()
            .into_bytes()
    }

    const RESUME: u64 = 1_793_491_200; // 2026-11-01T00:00:00Z

    fn spend_msg(prefix: &str) -> String {
        format!("{prefix}. You will regain access on 2026-11-01 at 00:00 UTC.")
    }

    fn kind(status: u16, body: &[u8]) -> Option<RefusalKind> {
        classify(status, body).map(|r| r.kind)
    }

    #[test]
    fn spend_limit_org_and_workspace_classify_with_resume_time() {
        for prefix in SPEND_LIMIT_PREFIXES {
            let body = envelope("invalid_request_error", &spend_msg(prefix));
            let refusal = classify(400, &body).expect("spend limit");
            assert_eq!(refusal.kind, RefusalKind::SpendLimit);
            assert_eq!(refusal.resume_at_epoch_secs, Some(RESUME));
        }
    }

    #[test]
    fn spend_limit_needs_status_400() {
        let body = envelope("invalid_request_error", &spend_msg(SPEND_LIMIT_PREFIXES[0]));
        assert_eq!(kind(400, &body), Some(RefusalKind::SpendLimit));
        assert_eq!(kind(402, &body), None);
        assert_eq!(kind(429, &body), None);
    }

    #[test]
    fn credit_balance_classifies_on_400_and_402() {
        let body = envelope(
            "invalid_request_error",
            "Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing.",
        );
        assert_eq!(kind(400, &body), Some(RefusalKind::CreditBalance));
        assert_eq!(kind(402, &body), Some(RefusalKind::CreditBalance));
        assert_eq!(kind(500, &body), None);
    }

    #[test]
    fn billing_error_classifies_on_any_402_message_only() {
        let body = envelope("billing_error", "anything at all");
        assert_eq!(kind(402, &body), Some(RefusalKind::Billing));
        assert_eq!(kind(400, &body), None);
    }

    #[test]
    fn usage_limits_substring_and_validation_400_stay_terminal() {
        let substring = envelope(
            "invalid_request_error",
            "max_tokens exceeds the usage limits of this model",
        );
        assert_eq!(kind(400, &substring), None);
        let validation = envelope("invalid_request_error", "messages: field required");
        assert_eq!(kind(400, &validation), None);
        // Positive twin: the real prefix on the same status is a refusal.
        let real = envelope("invalid_request_error", &spend_msg(SPEND_LIMIT_PREFIXES[0]));
        assert!(classify(400, &real).is_some());
    }

    #[test]
    fn non_json_html_bom_and_missing_fields_are_not_refusals() {
        let good = envelope("invalid_request_error", &spend_msg(SPEND_LIMIT_PREFIXES[0]));
        assert!(classify(400, &good).is_some());
        assert_eq!(classify(400, b"<html>Bad Request</html>"), None);
        assert_eq!(classify(400, b"not json"), None);
        assert_eq!(classify(400, b""), None);
        assert_eq!(classify(400, br#"{"type":"error"}"#), None);
        assert_eq!(classify(400, br#"{"error":{"type":"x"}}"#), None);
        // serde_json rejects a UTF-8 BOM prefix, so such a body stays terminal.
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend_from_slice(&good);
        assert_eq!(classify(400, &bom), None);
    }

    #[test]
    fn skip_window_uses_future_resume_else_one_hour() {
        let now = UNIX_EPOCH + Duration::from_secs(RESUME - 600);
        let future = AccountRefusal {
            kind: RefusalKind::SpendLimit,
            resume_at_epoch_secs: Some(RESUME),
        };
        assert_eq!(future.skip_window(now), Duration::from_secs(600));

        let past = AccountRefusal {
            resume_at_epoch_secs: Some(RESUME - 1200),
            ..future
        };
        assert_eq!(past.skip_window(now), REFUSAL_DEFAULT_SKIP);
        let missing = AccountRefusal {
            resume_at_epoch_secs: None,
            ..future
        };
        assert_eq!(missing.skip_window(now), REFUSAL_DEFAULT_SKIP);
    }

    #[test]
    fn unreadable_resume_time_yields_default_window() {
        for tail in [
            "",
            " You will regain access on soon.",
            " You will regain access on 2026-13-45 at 99:99 UTC.",
            " You will regain access on 2026-11-01 at 00:00.",
        ] {
            let msg = format!("{}.{tail}", SPEND_LIMIT_PREFIXES[0]);
            let refusal = classify(400, &envelope("invalid_request_error", &msg)).unwrap();
            assert_eq!(refusal.resume_at_epoch_secs, None, "{tail}");
            assert_eq!(refusal.skip_window(UNIX_EPOCH), REFUSAL_DEFAULT_SKIP);
        }
    }
}
