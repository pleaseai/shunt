//! Observation-only store for polled upstream provider Statuspage results
//! (`[server.status]`). Purely informational: the store is read by a metric
//! and the admin dashboard, and is never consulted by routing, failover, or
//! pool/cooldown decisions. See [`crate::status_poll`] for the background
//! poller that populates it.

use std::{collections::HashMap, sync::Mutex};

use serde::{Deserialize, Serialize};

/// Statuspage severity, plus the shunt-specific `Unknown` state for "we have
/// no signal" — returned for a fetch failure, a non-2xx response, an
/// oversized body, a JSON parse failure, or an unrecognized `status.indicator`
/// string.
///
/// `Unknown` must never collapse to `None`: `None` means "upstream reports
/// operational", `Unknown` means "we could not ask". Mapping a failure to
/// `None` would be a false all-clear, which is the specific bug this type
/// exists to make unrepresentable at the call sites that matter (the poller
/// and the metric).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Indicator {
    None,
    Minor,
    Major,
    Critical,
    Unknown,
}

impl Indicator {
    fn from_wire(value: &str) -> Self {
        match value {
            "none" => Indicator::None,
            "minor" => Indicator::Minor,
            "major" => Indicator::Major,
            "critical" => Indicator::Critical,
            _ => Indicator::Unknown,
        }
    }

    /// Numeric severity for the `shunt.upstream.status` gauge. `Unknown` has
    /// no severity: callers must omit the sample entirely rather than report
    /// one (see `metrics::record_upstream_status`), so this only covers the
    /// four Statuspage indicators.
    pub fn severity(self) -> Option<u8> {
        match self {
            Indicator::None => Some(0),
            Indicator::Minor => Some(1),
            Indicator::Major => Some(2),
            Indicator::Critical => Some(3),
            Indicator::Unknown => None,
        }
    }
}

/// One active incident, trimmed to the fields the dashboard renders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Incident {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub shortlink: String,
}

/// A provider's most recently observed status. A failed poll replaces any
/// previous good value with an `Unknown` entry — stale "operational" is
/// worse than "no signal" (see [`StatusStore::set`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpstreamStatus {
    pub indicator: Indicator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incidents: Vec<Incident>,
    /// The Statuspage `page.updated_at` timestamp, passed through verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_updated_at: Option<String>,
    /// Unix seconds when shunt completed this poll.
    pub observed_at: u64,
    /// Truncated (~200 char) failure detail, set only when `indicator` is
    /// `Unknown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl UpstreamStatus {
    /// Build an `Unknown` entry for a poll that could not produce a signal
    /// (transport error, non-2xx, oversized body). `detail` is truncated to
    /// the same ~200 chars as [`crate::auth::claude::usage::fetch_usage`]'s
    /// error path.
    pub fn unknown(observed_at: u64, detail: impl Into<String>) -> Self {
        Self {
            indicator: Indicator::Unknown,
            description: None,
            incidents: Vec::new(),
            page_updated_at: None,
            observed_at,
            error: Some(truncate(detail.into())),
        }
    }
}

fn truncate(mut detail: String) -> String {
    if let Some((index, _)) = detail.char_indices().nth(200) {
        detail.truncate(index);
    }
    detail
}

/// Mirrors Statuspage's `GET /api/v2/summary.json` shape, defensively: every
/// field is `#[serde(default)]` so upstream schema drift degrades to missing
/// data rather than a hard parse error.
#[derive(Debug, Default, Deserialize)]
struct SummaryResponse {
    #[serde(default)]
    page: SummaryPage,
    #[serde(default)]
    incidents: Vec<SummaryIncident>,
    #[serde(default)]
    status: SummaryStatus,
}

#[derive(Debug, Default, Deserialize)]
struct SummaryPage {
    #[serde(default)]
    updated_at: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct SummaryIncident {
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    shortlink: String,
}

#[derive(Debug, Default, Deserialize)]
struct SummaryStatus {
    #[serde(default)]
    indicator: String,
    #[serde(default)]
    description: Option<String>,
}

/// Parse a Statuspage `summary.json` body into an observed status.
///
/// Returns an `Unknown` entry (never `None`) when the body is not valid
/// JSON, or when `status.indicator` is not one of the four Statuspage
/// strings (`none`/`minor`/`major`/`critical`) — an unrecognized indicator is
/// exactly as much "no signal" as a request that failed outright.
pub fn parse_summary(body: &str, observed_at: u64) -> UpstreamStatus {
    let parsed: SummaryResponse = match serde_json::from_str(body) {
        Ok(parsed) => parsed,
        Err(error) => {
            return UpstreamStatus::unknown(
                observed_at,
                format!("invalid status response: {error}"),
            )
        }
    };
    let indicator = Indicator::from_wire(&parsed.status.indicator);
    if indicator == Indicator::Unknown {
        return UpstreamStatus::unknown(
            observed_at,
            format!(
                "unrecognized status indicator: {:?}",
                parsed.status.indicator
            ),
        );
    }
    UpstreamStatus {
        indicator,
        description: parsed.status.description,
        incidents: parsed
            .incidents
            .into_iter()
            .map(|incident| Incident {
                name: incident.name,
                status: incident.status,
                shortlink: incident.shortlink,
            })
            .collect(),
        page_updated_at: parsed.page.updated_at,
        observed_at,
        error: None,
    }
}

/// Process-lifetime store of the most recently observed status per provider,
/// held as `Arc<StatusStore>` on `AppState` alongside `accounts` so it
/// survives config reloads. Mirrors `AccountPool`'s plain-mutex shape: this
/// is low-frequency, low-cardinality state, not a hot path.
#[derive(Debug, Default)]
pub struct StatusStore {
    entries: Mutex<HashMap<String, UpstreamStatus>>,
}

impl StatusStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the stored entry for `provider`. A failed poll's `Unknown`
    /// result replaces any previous good value — stale "operational" is
    /// worse than "no signal", so there is no merge/keep-last-good path here.
    pub fn set(&self, provider: &str, status: UpstreamStatus) {
        self.entries
            .lock()
            .expect("status store lock poisoned")
            .insert(provider.to_string(), status);
    }

    /// Snapshot every stored entry, keyed by provider name.
    pub fn snapshot(&self) -> HashMap<String, UpstreamStatus> {
        self.entries
            .lock()
            .expect("status store lock poisoned")
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real shape returned by `https://status.claude.com/api/v2/summary.json`
    /// (fields trimmed to what shunt reads).
    const REAL_SUMMARY_JSON: &str = r#"{
        "page": {"id": "abc123", "name": "Claude", "updated_at": "2026-08-12T14:25:14.330Z"},
        "components": [{"id": "rwppv331jlwc", "name": "claude.ai", "status": "degraded_performance"}],
        "incidents": [{"id": "rk6gkg2gwfny", "name": "Degraded performance for multiple models", "status": "investigating", "impact": "minor", "shortlink": "https://stspg.io/abc"}],
        "status": {"indicator": "minor", "description": "Partially Degraded Service"}
    }"#;

    #[test]
    fn parses_the_real_summary_json_shape() {
        let status = parse_summary(REAL_SUMMARY_JSON, 1_000);
        assert_eq!(status.indicator, Indicator::Minor);
        assert_eq!(
            status.description.as_deref(),
            Some("Partially Degraded Service")
        );
        assert_eq!(status.incidents.len(), 1);
        assert_eq!(
            status.incidents[0].name,
            "Degraded performance for multiple models"
        );
        assert_eq!(
            status.page_updated_at.as_deref(),
            Some("2026-08-12T14:25:14.330Z")
        );
        assert_eq!(status.observed_at, 1_000);
        assert!(status.error.is_none());
    }

    #[test]
    fn malformed_json_is_unknown_never_none() {
        let status = parse_summary("not json", 1_000);
        assert_eq!(status.indicator, Indicator::Unknown);
        assert_ne!(status.indicator, Indicator::None);
        assert!(status.error.is_some());
    }

    #[test]
    fn unrecognized_indicator_string_is_unknown_never_none() {
        let body = r#"{"status": {"indicator": "maintenance", "description": null}}"#;
        let status = parse_summary(body, 1_000);
        assert_eq!(status.indicator, Indicator::Unknown);
        assert_ne!(status.indicator, Indicator::None);
        assert!(status.error.is_some());
    }

    #[test]
    fn a_failed_poll_replaces_a_previous_good_value_with_unknown() {
        let store = StatusStore::new();
        store.set(
            "claude",
            UpstreamStatus {
                indicator: Indicator::None,
                description: Some("All Systems Operational".to_string()),
                incidents: Vec::new(),
                page_updated_at: None,
                observed_at: 1_000,
                error: None,
            },
        );
        store.set(
            "claude",
            UpstreamStatus::unknown(2_000, "connection refused"),
        );
        let snapshot = store.snapshot();
        let status = snapshot.get("claude").expect("entry present");
        assert_eq!(status.indicator, Indicator::Unknown);
        assert_ne!(status.indicator, Indicator::None);
        assert_eq!(status.observed_at, 2_000);
    }

    #[test]
    fn unknown_error_detail_is_truncated() {
        let long = "x".repeat(500);
        let status = UpstreamStatus::unknown(1_000, long);
        assert_eq!(status.error.unwrap().chars().count(), 200);
    }
}
