//! Error-code table for OpenAI's Sign in with ChatGPT (SIWC) subscription
//! sharing preview (ADR-0007 section 4).
//!
//! One plain lookup feeds both the in-stream mapper (`backend_error_status`
//! for `response.failed` / `error` events) and the HTTP mapper
//! (`map_error_value`). It holds no account state: pool rotation and cooldown
//! live elsewhere. Codes outside the table return `None`, so every other
//! provider keeps its status-based mapping unchanged.

use axum::http::StatusCode;
use serde_json::Value;

/// Prefix shared by every ChatPass v2 authorization failure
/// (`chatpass_v2_scope_not_authorized`, `chatpass_v2_invalid_authorization_context`, ...).
const CHATPASS_V2_PREFIX: &str = "chatpass_v2_";

/// The status and Anthropic error type a SIWC code maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SiwcError {
    pub status: StatusCode,
    pub error_type: &'static str,
}

/// Look up a SIWC error code. `None` for any code this table does not own.
pub fn lookup(code: &str) -> Option<SiwcError> {
    let (status, error_type) = match code {
        "subscription_sharing_usage_limit_exceeded" => {
            (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error")
        }
        // Overload, not `api_error`: the account is out of capacity for now,
        // which clients back off and retry.
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => {
            (StatusCode::SERVICE_UNAVAILABLE, "overloaded_error")
        }
        "subscription_sharing_unsupported_capability" => {
            (StatusCode::BAD_REQUEST, "invalid_request_error")
        }
        "subscription_sharing_user_not_eligible" | "subscription_sharing_route_not_supported" => {
            (StatusCode::FORBIDDEN, "permission_error")
        }
        "subscription_sharing_invalid_user" => (StatusCode::UNAUTHORIZED, "authentication_error"),
        _ if code.starts_with(CHATPASS_V2_PREFIX) => (StatusCode::FORBIDDEN, "permission_error"),
        _ => return None,
    };
    Some(SiwcError { status, error_type })
}

/// Append `(parameter: <param>)` to `message` when the payload names the
/// offending request parameter, at `/error/param` or `/response/error/param`.
/// `None` when absent or empty, leaving the message untouched.
pub fn message_with_param(value: &Value, message: &str) -> Option<String> {
    let param = value
        .pointer("/error/param")
        .or_else(|| value.pointer("/response/error/param"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|param| !param.is_empty())?;
    Some(format!("{message} (parameter: {param})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn table_maps_each_code_to_status_and_type() {
        let cases = [
            (
                "subscription_sharing_usage_limit_exceeded",
                429,
                "rate_limit_error",
            ),
            (
                "subscription_sharing_usage_unavailable",
                503,
                "overloaded_error",
            ),
            (
                "subscription_sharing_user_unavailable",
                503,
                "overloaded_error",
            ),
            (
                "subscription_sharing_unsupported_capability",
                400,
                "invalid_request_error",
            ),
            (
                "subscription_sharing_user_not_eligible",
                403,
                "permission_error",
            ),
            (
                "subscription_sharing_route_not_supported",
                403,
                "permission_error",
            ),
            (
                "subscription_sharing_invalid_user",
                401,
                "authentication_error",
            ),
            ("chatpass_v2_scope_not_authorized", 403, "permission_error"),
            (
                "chatpass_v2_invalid_authorization_context",
                403,
                "permission_error",
            ),
        ];
        for (code, status, kind) in cases {
            let hit = lookup(code).unwrap_or_else(|| panic!("{code} missing"));
            assert_eq!(hit.status.as_u16(), status, "{code}");
            assert_eq!(hit.error_type, kind, "{code}");
        }
    }

    #[test]
    fn unknown_codes_are_not_owned() {
        for code in [
            "",
            "rate_limit_exceeded",
            "subscription_sharing_other",
            "chatpass_v1_x",
        ] {
            assert_eq!(lookup(code), None, "{code}");
        }
    }

    #[test]
    fn param_suffix_reads_both_shapes() {
        let http = json!({"error": {"param": "tools"}});
        let stream = json!({"response": {"error": {"param": "tools"}}});
        assert_eq!(
            message_with_param(&http, "m").as_deref(),
            Some("m (parameter: tools)")
        );
        assert_eq!(
            message_with_param(&stream, "m").as_deref(),
            Some("m (parameter: tools)")
        );
        assert_eq!(
            message_with_param(&json!({"error": {"param": ""}}), "m"),
            None
        );
        assert_eq!(message_with_param(&json!({}), "m"), None);
    }
}
