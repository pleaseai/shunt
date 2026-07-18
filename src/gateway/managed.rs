use std::collections::HashMap;

use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::{error::ShuntError, server::AppState};

const TELEMETRY_ENV: [(&str, &str); 5] = [
    ("CLAUDE_CODE_ENABLE_TELEMETRY", "1"),
    ("OTEL_METRICS_EXPORTER", "otlp"),
    ("OTEL_LOGS_EXPORTER", "otlp"),
    ("OTEL_TRACES_EXPORTER", "otlp"),
    ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/protobuf"),
];

#[derive(Clone, Debug)]
pub(crate) struct ResolvedPolicy {
    pub(crate) emails: Option<Vec<String>>,
    pub(crate) settings: Value,
}

pub(crate) fn resolve_all(
    policies: &[ResolvedPolicy],
    telemetry_push: bool,
    public_url: &str,
) -> (Value, HashMap<String, Value>) {
    let mut catch_all = Value::Object(Map::new());
    for policy in policies.iter().filter(|policy| policy.emails.is_none()) {
        merge(&mut catch_all, &policy.settings, None);
    }

    let mut by_email = HashMap::new();
    for policy in policies.iter().filter(|policy| policy.emails.is_some()) {
        for email in policy.emails.as_ref().expect("filtered user policy") {
            if by_email.contains_key(email) {
                tracing::debug!(
                    email,
                    "gateway email already matched by an earlier policy; skipping"
                );
                continue;
            }
            let mut settings = catch_all.clone();
            merge(&mut settings, &policy.settings, None);
            by_email.insert(
                email.clone(),
                inject_telemetry(settings, telemetry_push, public_url),
            );
        }
    }

    (
        inject_telemetry(catch_all, telemetry_push, public_url),
        by_email,
    )
}

fn inject_telemetry(mut settings: Value, telemetry_push: bool, public_url: &str) -> Value {
    if telemetry_push {
        let mut env = Map::new();
        for (key, value) in TELEMETRY_ENV {
            env.insert(key.to_string(), Value::String(value.to_string()));
        }
        env.insert(
            "OTEL_EXPORTER_OTLP_ENDPOINT".to_string(),
            Value::String(public_url.trim_end_matches('/').to_string()),
        );
        let mut injected = json!({ "env": env });
        merge(&mut injected, &settings, None);
        settings = injected;
    }
    settings
}

pub(crate) fn available_models(settings: &Value) -> Option<&[Value]> {
    settings
        .get("availableModels")?
        .as_array()
        .map(Vec::as_slice)
}

pub(crate) fn merge(base: &mut Value, overlay: &Value, key: Option<&str>) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (child_key, overlay_value) in overlay {
                match base.get_mut(child_key) {
                    Some(base_value) => merge(base_value, overlay_value, Some(child_key)),
                    None => {
                        let mut inserted = match overlay_value {
                            Value::Object(_) => Value::Object(Map::new()),
                            Value::Array(_) if child_key.to_ascii_lowercase().contains("deny") => {
                                Value::Array(Vec::new())
                            }
                            _ => overlay_value.clone(),
                        };
                        merge(&mut inserted, overlay_value, Some(child_key));
                        base.insert(child_key.clone(), inserted);
                    }
                }
            }
        }
        (Value::Array(base), Value::Array(overlay))
            if key.is_some_and(|key| key.to_ascii_lowercase().contains("deny")) =>
        {
            for item in overlay {
                if !base.contains(item) {
                    base.push(item.clone());
                }
            }
        }
        (base, overlay) => *base = overlay.clone(),
    }
}

pub async fn get(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let state = state.refreshed();
    let Some(auth) = state.gateway_auth else {
        return ShuntError::new(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "missing or invalid gateway bearer token",
        )
        .into_response();
    };
    let Some(claims) = auth.authenticate_bearer(&headers) else {
        return ShuntError::new(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "missing or invalid gateway bearer token",
        )
        .into_response();
    };
    let Some(settings) = auth.managed_settings(&claims.email) else {
        return ShuntError::new(
            StatusCode::NOT_FOUND,
            "not_found_error",
            "no managed policy",
        )
        .into_response();
    };

    let settings_bytes = serde_json::to_vec(settings).expect("managed settings serialize");
    let checksum = sha256_label(&settings_bytes);
    let quoted_etag = format!("\"{checksum}\"");
    let etag = HeaderValue::from_str(&quoted_etag).expect("SHA-256 ETag is a valid header value");
    if if_none_match(&headers, &checksum) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        response.headers_mut().insert(header::ETAG, etag);
        return response;
    }
    let uuid = sha256_label(format!("shunt-managed-settings:{}", claims.sub).as_bytes());
    let mut response = Json(json!({
        "uuid": uuid,
        "checksum": checksum,
        "settings": settings,
    }))
    .into_response();
    response.headers_mut().insert(header::ETAG, etag);
    response
}

fn sha256_label(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn if_none_match(headers: &HeaderMap, checksum: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|candidate| {
            if candidate == "*" {
                return true;
            }
            let candidate = candidate
                .strip_prefix("W/")
                .or_else(|| candidate.strip_prefix("w/"))
                .unwrap_or(candidate)
                .trim();
            candidate.trim_matches('"') == checksum
        })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::merge;

    #[test]
    fn merge_replaces_allow_lists_unions_denies_and_merges_records() {
        let mut base = json!({
            "availableModels": ["base"],
            "permissions": {"allow": ["Read"], "deny": ["Bash", "WebFetch"]},
            "env": {"BASE": "1", "SHARED": "base"},
            "nested": {"left": true}
        });
        merge(
            &mut base,
            &json!({
                "availableModels": ["overlay"],
                "permissions": {"allow": ["Edit"], "deny": ["WebFetch", "Write"]},
                "env": {"OVERLAY": "1", "SHARED": "overlay"},
                "nested": {"right": true}
            }),
            None,
        );
        assert_eq!(
            base,
            json!({
                "availableModels": ["overlay"],
                "permissions": {
                    "allow": ["Edit"],
                    "deny": ["Bash", "WebFetch", "Write"]
                },
                "env": {"BASE": "1", "OVERLAY": "1", "SHARED": "overlay"},
                "nested": {"left": true, "right": true}
            })
        );
    }

    #[test]
    fn merge_deduplicates_deny_arrays_introduced_with_new_objects() {
        let mut base = json!({});
        merge(
            &mut base,
            &json!({
                "permissions": {"deny": ["Bash", "Bash", "Write"]},
                "customDenyList": ["first", "first", "second"]
            }),
            None,
        );

        assert_eq!(
            base,
            json!({
                "permissions": {"deny": ["Bash", "Write"]},
                "customDenyList": ["first", "second"]
            })
        );
    }

    #[test]
    fn merge_unions_deny_arrays_case_insensitively() {
        let mut base = json!({
            "permissions": {"DeNy": ["Bash", "WebFetch"]},
            "customDenyList": ["first"]
        });
        merge(
            &mut base,
            &json!({
                "permissions": {"DeNy": ["WebFetch", "Write"]},
                "customDenyList": ["first", "second"]
            }),
            None,
        );

        assert_eq!(
            base,
            json!({
                "permissions": {"DeNy": ["Bash", "WebFetch", "Write"]},
                "customDenyList": ["first", "second"]
            })
        );
    }
}
