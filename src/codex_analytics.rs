//! Accept-and-discard sink for Codex CLI product analytics.
//!
//! The Codex CLI posts batches shaped as `{"events":[{"event_type":...}]}` to
//! its configured `chatgpt_base_url`. shunt accepts those requests and records
//! only one counter per sanitized event name. It never forwards analytics
//! upstream because a pooled account would misattribute client telemetry to the
//! account selected by shunt. The payload is never logged or exported; only
//! sanitized event names become metric attributes, and the number of *distinct*
//! names is capped so a client cannot inflate the `event` attribute's
//! cardinality without bound. A body that cannot be read (oversized batch or a
//! transport error) records a distinct `read_error`, and a body that is not a
//! recognizable batch records `unparsed`; either way the request still returns
//! `200` so the Codex CLI never treats analytics delivery as a hard failure.

use std::{
    collections::HashSet,
    sync::{Mutex, OnceLock},
};

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
/// Upper bound on the number of distinct sanitized event names ever emitted as
/// the `event` attribute. Well beyond the handful of real Codex event types, so
/// legitimate traffic is never folded, but finite so a hostile or buggy client
/// cannot drive unbounded metric-series cardinality.
const MAX_DISTINCT_EVENTS: usize = 64;
const UNPARSED_EVENT: &str = "unparsed";
const READ_ERROR_EVENT: &str = "read_error";
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

    // A body-read failure (over the 8 MiB cap or a transport error) is counted
    // distinctly from an unparseable body, so an operator can tell "Codex
    // changed its payload" from "clients keep exceeding the limit"; both still
    // return 200 to honor the accept-and-discard contract.
    let read = to_bytes(body, MAX_ANALYTICS_BODY_BYTES).await;
    let event_names = names_for_body(read.as_deref().map_err(|_| ()));

    for event_name in &event_names {
        crate::metrics::record_codex_client_event(&bounded_event_name(event_name));
    }

    tracing::debug!(
        event_count = event_names.len(),
        "accepted inbound codex analytics events"
    );
    (StatusCode::OK, Json(serde_json::json!({}))).into_response()
}

/// The event names to record for one request body, one entry per event. `Err`
/// (body unreadable — oversized or a transport failure) yields a single
/// `read_error`; a body that is not a `{"events": [...]}` batch yields a single
/// `unparsed`; a batch yields one raw event name per event. The returned names
/// are not yet sanitized or cardinality-capped — the caller applies
/// `bounded_event_name` per name at record time.
fn names_for_body(read: Result<&[u8], ()>) -> Vec<String> {
    match read {
        Err(()) => vec![READ_ERROR_EVENT.to_owned()],
        Ok(body) => match parse_event_names(body) {
            Some(names) => names,
            None => vec![UNPARSED_EVENT.to_owned()],
        },
    }
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

/// Sanitize a raw event name, then fold it to `other` once the process-wide
/// distinct-name budget is spent. Reserved reason codes (`other`, `unparsed`,
/// `read_error`) are fixed and never consume the budget.
fn bounded_event_name(event: &str) -> String {
    let sanitized = sanitize_event_name(event);
    if is_reserved(sanitized) {
        return sanitized.to_owned();
    }
    let mut seen = distinct_event_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    bound_within_cap(&mut seen, sanitized).to_owned()
}

/// The three fixed reason codes shunt itself emits — bounded by construction.
fn is_reserved(name: &str) -> bool {
    name == OTHER_EVENT || name == UNPARSED_EVENT || name == READ_ERROR_EVENT
}

/// Admit `sanitized` while distinct names remain under `MAX_DISTINCT_EVENTS`;
/// once the budget is spent, previously-seen names still pass but any novel name
/// folds to `other`, capping the `event` attribute's cardinality.
fn bound_within_cap<'a>(seen: &mut HashSet<String>, sanitized: &'a str) -> &'a str {
    if seen.contains(sanitized) {
        sanitized
    } else if seen.len() < MAX_DISTINCT_EVENTS {
        seen.insert(sanitized.to_owned());
        sanitized
    } else {
        OTHER_EVENT
    }
}

/// Process-wide set of distinct sanitized event names already admitted as the
/// `event` attribute. Never cleared: the cap is a lifetime bound on series
/// cardinality, not a per-request one.
fn distinct_event_registry() -> &'static Mutex<HashSet<String>> {
    static REGISTRY: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashSet::new()))
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
    use super::{
        bound_within_cap, names_for_body, parse_event_names, sanitize_event_name,
        MAX_DISTINCT_EVENTS, OTHER_EVENT,
    };
    use std::collections::HashSet;

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

    #[test]
    fn parses_event_type_and_alternate_name_fields() {
        // `event_type` wins, then `event_name`, then `name`; an event with none
        // of the three degrades to `other` without dropping the rest of the batch.
        let body = br#"{"events":[
            {"event_type":"codex.turn_started"},
            {"event_name":"codex.turn_completed"},
            {"name":"codex.tool_call"},
            {"unrelated":"x"}
        ]}"#;
        assert_eq!(
            parse_event_names(body),
            Some(vec![
                "codex.turn_started".to_owned(),
                "codex.turn_completed".to_owned(),
                "codex.tool_call".to_owned(),
                "other".to_owned(),
            ])
        );
    }

    #[test]
    fn event_type_takes_precedence_over_alternate_fields() {
        let body =
            br#"{"events":[{"event_type":"primary","event_name":"secondary","name":"tertiary"}]}"#;
        assert_eq!(parse_event_names(body), Some(vec!["primary".to_owned()]));
    }

    #[test]
    fn non_batch_bodies_are_unparsed() {
        assert_eq!(parse_event_names(b"{not-json"), None);
        assert_eq!(parse_event_names(br#"{"other":true}"#), None);
        assert_eq!(parse_event_names(br#"{"events":"nope"}"#), None);
    }

    #[test]
    fn names_for_body_records_one_name_per_event() {
        let body = br#"{"events":[{"event_type":"codex.turn_started"},{"event_type":"codex.turn_completed"}]}"#;
        assert_eq!(
            names_for_body(Ok(body)),
            vec![
                "codex.turn_started".to_owned(),
                "codex.turn_completed".to_owned()
            ]
        );
    }

    #[test]
    fn names_for_body_distinguishes_read_error_from_unparsed() {
        assert_eq!(names_for_body(Err(())), vec!["read_error".to_owned()]);
        assert_eq!(
            names_for_body(Ok(b"{not-json")),
            vec!["unparsed".to_owned()]
        );
    }

    #[test]
    fn cap_folds_novel_names_once_the_budget_is_spent() {
        let mut seen = HashSet::new();
        // Fill the budget with distinct admitted names.
        for i in 0..MAX_DISTINCT_EVENTS {
            let name = format!("event-{i}");
            assert_eq!(bound_within_cap(&mut seen, &name), name);
        }
        // A previously-seen name still passes even though the budget is spent.
        assert_eq!(bound_within_cap(&mut seen, "event-0"), "event-0");
        // A novel name now folds to `other`.
        assert_eq!(bound_within_cap(&mut seen, "event-new"), OTHER_EVENT);
        assert_eq!(seen.len(), MAX_DISTINCT_EVENTS);
    }
}
