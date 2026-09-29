//! Which gated-call refusals are a context-window overflow (issue #654).
//!
//! libsy's own client types that refusal as
//! `LlmClientError::ContextWindowExceeded` rather than a generic
//! `UpstreamHttp`, and the pinned algorithms branch on it: `escalation` falls
//! back to its strong target, and `advisor` propagates it so the host can
//! relay the `400` a harness compacts on. Hosting libsy, shunt serves the
//! gated call itself, so the typing has to happen here or the escalation
//! fallback never runs.
//!
//! The rule is the one the pinned libsy client applies
//! (`libsy-llm-client/src/backend.rs` `is_context_overflow`, `error.rs`
//! `is_overflow_body`, rev `3ddea9d`): a `400` whose error message — or,
//! failing that, the raw body, whether or not it parsed as JSON — contains one
//! of the phrases, case insensitively. The gated call's refusal is the one the caller would have
//! been handed, rendered in the Anthropic Messages error shape by either
//! adapter (the Responses adapter rewrites an upstream overflow into
//! Anthropic's "prompt is too long" wording), so the Anthropic backend's
//! phrase set is the one that applies. Any other status, or a `400` naming
//! anything else, stays a plain refusal.

use axum::http::StatusCode;
use serde_json::Value;

/// libsy's `ANTHROPIC_OVERFLOW_PHRASES`, verbatim.
const OVERFLOW_PHRASES: &[&str] = &[
    "prompt is too long",
    "maximum number of tokens",
    "context window",
    "context length",
];

/// Whether a gated call's refusal is a context-window overflow.
pub(crate) fn is_context_overflow(status: StatusCode, body: &str) -> bool {
    if status != StatusCode::BAD_REQUEST {
        return false;
    }
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if value
            .pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(contains_phrase)
        {
            return true;
        }
    }
    // Some upstream proxies answer in plain text; libsy falls through to the
    // raw body for the same reason — unconditionally, even for a JSON body
    // whose `error.message` names nothing, so this does too.
    contains_phrase(body)
}

fn contains_phrase(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    OVERFLOW_PHRASES.iter().any(|phrase| lower.contains(phrase))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// libsy's own `anthropic_detects_prompt_too_long` cases, the shapes the
    /// two adapters render, and the refusals that must stay plain.
    #[test]
    fn an_overflow_is_a_400_naming_the_context_window() {
        let overflow = [
            r#"{"error":{"message":"prompt is too long: 200000 tokens"}}"#,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"Input exceeds the Context Window of this model"}}"#,
            "upstream proxy: prompt is too long",
        ];
        for body in overflow {
            assert!(is_context_overflow(StatusCode::BAD_REQUEST, body), "{body}");
        }
        let plain = [
            r#"{"error":{"message":"overloaded"}}"#,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: must be at most 8192"}}"#,
            "",
        ];
        for body in plain {
            assert!(
                !is_context_overflow(StatusCode::BAD_REQUEST, body),
                "{body}"
            );
        }
        // The status is part of the rule: the same body on another status is
        // not the refusal libsy types.
        let body = r#"{"error":{"message":"prompt is too long: 200000 tokens"}}"#;
        for status in [StatusCode::PAYLOAD_TOO_LARGE, StatusCode::BAD_GATEWAY] {
            assert!(!is_context_overflow(status, body), "{status}");
        }
    }

    /// libsy's `is_overflow_body` falls through to the raw body even when the
    /// JSON parsed and its `error.message` names nothing: a phrase anywhere
    /// else in the body still types the refusal. Returning on the message
    /// alone would diverge from the pinned client.
    #[test]
    fn a_phrase_outside_the_error_message_still_matches_as_libsy_does() {
        let body = r#"{"error":{"message":"invalid request","detail":"input exceeds the context window"}}"#;
        assert!(is_context_overflow(StatusCode::BAD_REQUEST, body));
    }
}
