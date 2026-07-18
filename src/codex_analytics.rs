//! Accept-and-discard sink for Codex CLI product analytics.
//!
//! The Codex CLI posts batches shaped as `{"events":[{"event_type":...}]}` to
//! its configured `chatgpt_base_url`. shunt accepts those requests and records
//! only one counter per sanitized event name. It never forwards analytics
//! upstream because a pooled account would misattribute client telemetry to the
//! account selected by shunt. The payload is never logged or exported; only
//! sanitized event names become metric attributes.

use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::Value;

use crate::{error::ShuntError, server::AppState};

const MAX_ANALYTICS_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_EVENT_NAME_BYTES: usize = 64;
const UNPARSED_EVENT: &str = "unparsed";
const OTHER_EVENT: &str = "other";

/// Accept one Codex analytics batch without retaining or forwarding its payload.
pub async fn post(State(state): State<AppState>, headers: HeaderMap, body: Body) -> Response {
    let state = state.refreshed();

    // Match the inbound Responses endpoint's auth posture: open without
    // `[server.auth]`, otherwise accept the configured header or Bearer token.
    if let Some(auth) = &state.inbound_auth {
        if auth.authenticate_bearer(&headers).is_none() {
            tracing::warn!("inbound codex analytics auth failed: missing or invalid client token");
            let message = format!(
                "missing or invalid client token for the inbound codex endpoint: provide it via the `{}` header or `Authorization: Bearer <token>` (e.g. OPENAI_API_KEY); ask the operator for one",
                auth.header()
            );
            let response =
                ShuntError::new(StatusCode::UNAUTHORIZED, "authentication_error", message)
                    .into_response();
            return crate::error::into_openai_error_shape(response).await;
        }
    }

    let event_names = to_bytes(body, MAX_ANALYTICS_BODY_BYTES)
        .await
        .ok()
        .and_then(|body| parse_event_names(&body));

    let event_count = match event_names {
        Some(event_names) => {
            for event_name in &event_names {
                crate::metrics::record_codex_client_event(sanitize_event_name(event_name));
            }
            event_names.len()
        }
        None => {
            crate::metrics::record_codex_client_event(UNPARSED_EVENT);
            1
        }
    };

    tracing::debug!(event_count, "accepted inbound codex analytics events");
    (StatusCode::OK, Json(serde_json::json!({}))).into_response()
}

/// `None` only when the body is not a `{"events": [...]}` batch at all; an
/// individual event without a recognizable name field degrades to `other`
/// instead of discarding the rest of the batch.
fn parse_event_names(body: &[u8]) -> Option<Vec<String>> {
    let payload: Value = serde_json::from_slice(body).ok()?;
    let events = payload.get("events")?.as_array()?;
    Some(
        events
            .iter()
            .map(|event| {
                event
                    .get("event_type")
                    .or_else(|| event.get("event_name"))
                    .or_else(|| event.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or(OTHER_EVENT)
                    .to_owned()
            })
            .collect(),
    )
}

/// Keep only non-empty event names of at most 64 bytes containing lowercase
/// ASCII letters, digits, `.`, `_`, or `-`; map every other name to `other`.
fn sanitize_event_name(event: &str) -> &str {
    if !event.is_empty()
        && event.len() <= MAX_EVENT_NAME_BYTES
        && event.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
    {
        event
    } else {
        OTHER_EVENT
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_event_name;

    #[test]
    fn sanitizer_preserves_allowed_names() {
        assert_eq!(
            sanitize_event_name("codex.turn_completed-2"),
            "codex.turn_completed-2"
        );
    }

    #[test]
    fn sanitizer_maps_bad_characters_to_other() {
        assert_eq!(sanitize_event_name("Codex Turn"), "other");
        assert_eq!(sanitize_event_name("codex/turn"), "other");
        assert_eq!(sanitize_event_name(""), "other");
    }

    #[test]
    fn sanitizer_maps_names_over_the_length_cap_to_other() {
        let at_cap = "a".repeat(64);
        let over_cap = "a".repeat(65);
        assert_eq!(sanitize_event_name(&at_cap), at_cap);
        assert_eq!(sanitize_event_name(&over_cap), "other");
    }
}
