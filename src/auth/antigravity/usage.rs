//! Antigravity pool quota client: Google's Code Assist `retrieveUserQuotaSummary` RPC.
//!
//! Antigravity presents two shared model-family quota pools: Gemini models and
//! Claude/GPT models. The summary RPC reports the provider-native windows for
//! those pools (currently 5-hour and weekly buckets) with independent remaining
//! fractions and reset times. This module flattens those grouped buckets into
//! the dashboard's existing `QuotaBucketSnapshot` shape without pretending that
//! every catalog model owns an independent quota.
//!
//! The endpoint is private/undocumented. Keep this display-only: unlike Claude
//! and Codex, Antigravity has no reactive quota headers that can be reconciled
//! into the pool's generic 5h/7d selection state.

use anyhow::Context;
use serde::Deserialize;

use crate::accounts::QuotaBucketSnapshot;

const GEMINI_5H_LABEL: &str = "Gemini Models · 5h";
const GEMINI_WEEKLY_LABEL: &str = "Gemini Models · weekly";
const OTHER_5H_LABEL: &str = "Claude + GPT Models · 5h";
const OTHER_WEEKLY_LABEL: &str = "Claude + GPT Models · weekly";

#[derive(Debug, Deserialize)]
struct QuotaSummaryResponse {
    #[serde(default)]
    groups: Vec<QuotaSummaryGroup>,
}

#[derive(Debug, Deserialize)]
struct QuotaSummaryGroup {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    #[serde(default)]
    buckets: Vec<QuotaSummaryBucket>,
}

#[derive(Debug, Deserialize)]
struct QuotaSummaryBucket {
    #[serde(rename = "bucketId")]
    bucket_id: Option<String>,
    window: Option<String>,
    #[serde(rename = "remainingFraction")]
    remaining_fraction: Option<f64>,
    #[serde(rename = "resetTime")]
    reset_time: Option<String>,
}

/// Fetch one Antigravity account's grouped model-family quota windows.
///
/// Production `cloudcode-pa.googleapis.com` configurations are normalized
/// through the same daily-host resolver used by Antigravity inference/catalog
/// traffic. Loopback/custom hosts remain untouched so tests and operator
/// proxies keep working.
pub async fn fetch_usage(
    client: &reqwest::Client,
    base_url: &str,
    access_token: &str,
    project_id: &str,
) -> anyhow::Result<Vec<QuotaBucketSnapshot>> {
    let url = quota_summary_url(base_url);
    let response = client
        .post(&url)
        .bearer_auth(access_token)
        .header("User-Agent", super::version::user_agent())
        .json(&serde_json::json!({ "project": project_id }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?;
    let status = response.status();
    let text = response
        .text()
        .await
        .context("Antigravity quota-summary response body read failed")?;
    if !status.is_success() {
        let detail: String = text.chars().take(200).collect();
        anyhow::bail!("quota-summary request failed ({status}): {detail}");
    }

    let summary: QuotaSummaryResponse = serde_json::from_str(&text)
        .map_err(|error| anyhow::anyhow!("invalid quota-summary response: {error}"))?;
    parse_usage(summary)
}

fn quota_summary_url(base_url: &str) -> String {
    let base = super::auth::inference_base_url(base_url);
    format!(
        "{}/{API_VERSION}:retrieveUserQuotaSummary",
        base.trim_end_matches('/'),
        API_VERSION = super::auth::API_VERSION,
    )
}

/// Flatten the two provider-native quota groups into at most four dashboard
/// windows. Unknown groups/windows are ignored rather than exposed as fake
/// independent quotas. A response with no numeric recognized bucket is rejected
/// so a malformed payload cannot replace the last good dashboard snapshot.
fn parse_usage(summary: QuotaSummaryResponse) -> anyhow::Result<Vec<QuotaBucketSnapshot>> {
    let mut out = Vec::new();

    for group in summary.groups {
        let group_name = group.display_name.as_deref().unwrap_or_default();
        for bucket in group.buckets {
            let Some(remaining) = bucket.remaining_fraction else {
                continue;
            };
            let Some(label) = canonical_label(
                group_name,
                bucket.bucket_id.as_deref(),
                bucket.window.as_deref(),
            ) else {
                continue;
            };
            if out
                .iter()
                .any(|existing: &QuotaBucketSnapshot| existing.label == label)
            {
                continue;
            }
            out.push(QuotaBucketSnapshot {
                label: label.to_string(),
                remaining: Some(remaining),
                reset_time: bucket.reset_time,
            });
        }
    }

    out.sort_by_key(|bucket| match bucket.label.as_str() {
        GEMINI_5H_LABEL => 0,
        GEMINI_WEEKLY_LABEL => 1,
        OTHER_5H_LABEL => 2,
        OTHER_WEEKLY_LABEL => 3,
        _ => 4,
    });

    if out.is_empty() {
        anyhow::bail!("quota-summary response carries no usable grouped quota buckets");
    }
    Ok(out)
}

fn canonical_label(
    group_name: &str,
    bucket_id: Option<&str>,
    window: Option<&str>,
) -> Option<&'static str> {
    let bucket_id = bucket_id.unwrap_or_default().to_ascii_lowercase();
    match bucket_id.as_str() {
        "gemini-5h" => return Some(GEMINI_5H_LABEL),
        "gemini-weekly" => return Some(GEMINI_WEEKLY_LABEL),
        "3p-5h" => return Some(OTHER_5H_LABEL),
        "3p-weekly" => return Some(OTHER_WEEKLY_LABEL),
        _ => {}
    }

    // Fail-soft fallback for cosmetic upstream id changes: preserve the two
    // model-family groups and only accept the two windows Antigravity exposes.
    let group = group_name.to_ascii_lowercase();
    let is_gemini = group.contains("gemini");
    let is_other = group.contains("claude") || group.contains("gpt") || group.contains("3p");
    let window = window
        .unwrap_or_default()
        .to_ascii_lowercase()
        .chars()
        .filter(|ch| !matches!(ch, ' ' | '-' | '_'))
        .collect::<String>();

    let is_5h = window == "5h" || window.contains("fivehour");
    let is_weekly = window == "weekly" || window.contains("week");

    match (is_gemini, is_other, is_5h, is_weekly) {
        (true, false, true, false) => Some(GEMINI_5H_LABEL),
        (true, false, false, true) => Some(GEMINI_WEEKLY_LABEL),
        (false, true, true, false) => Some(OTHER_5H_LABEL),
        (false, true, false, true) => Some(OTHER_WEEKLY_LABEL),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grouped_fixture() -> QuotaSummaryResponse {
        serde_json::from_value(serde_json::json!({
            "groups": [
                {
                    "displayName": "Gemini Models",
                    "buckets": [
                        {
                            "bucketId": "gemini-weekly",
                            "displayName": "Weekly Limit",
                            "window": "weekly",
                            "remainingFraction": 0.41,
                            "resetTime": "2026-09-30T12:00:00Z"
                        },
                        {
                            "bucketId": "gemini-5h",
                            "displayName": "Five Hour Limit",
                            "window": "5h",
                            "remainingFraction": 0.70,
                            "resetTime": "2026-09-24T22:00:00Z"
                        }
                    ]
                },
                {
                    "displayName": "Claude and GPT models",
                    "buckets": [
                        {
                            "bucketId": "3p-weekly",
                            "window": "weekly",
                            "remainingFraction": 0.63,
                            "resetTime": "2026-09-29T08:00:00Z"
                        },
                        {
                            "bucketId": "3p-5h",
                            "window": "5h",
                            "remainingFraction": 0.92,
                            "resetTime": "2026-09-24T23:00:00Z"
                        }
                    ]
                }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn parses_two_shared_model_pools_and_orders_their_windows() {
        let buckets = parse_usage(grouped_fixture()).expect("grouped summary parses");
        assert_eq!(buckets.len(), 4);
        assert_eq!(buckets[0].label, GEMINI_5H_LABEL);
        assert_eq!(buckets[0].remaining, Some(0.70));
        assert_eq!(buckets[1].label, GEMINI_WEEKLY_LABEL);
        assert_eq!(buckets[1].remaining, Some(0.41));
        assert_eq!(buckets[2].label, OTHER_5H_LABEL);
        assert_eq!(buckets[2].remaining, Some(0.92));
        assert_eq!(buckets[3].label, OTHER_WEEKLY_LABEL);
        assert_eq!(buckets[3].remaining, Some(0.63));
    }

    #[test]
    fn accepts_known_group_and_window_when_bucket_id_is_absent() {
        let summary: QuotaSummaryResponse = serde_json::from_value(serde_json::json!({
            "groups": [{
                "displayName": "Claude and GPT models",
                "buckets": [{
                    "window": "5h",
                    "remainingFraction": 0.5
                }]
            }]
        }))
        .unwrap();
        let buckets = parse_usage(summary).unwrap();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].label, OTHER_5H_LABEL);
    }

    #[test]
    fn rejects_summary_without_any_usable_bucket() {
        let summary: QuotaSummaryResponse = serde_json::from_value(serde_json::json!({
            "groups": [{
                "displayName": "Gemini Models",
                "buckets": [{
                    "bucketId": "gemini-5h",
                    "resetTime": "2026-09-24T22:00:00Z"
                }]
            }]
        }))
        .unwrap();
        assert!(parse_usage(summary).is_err());
    }

    #[test]
    fn quota_summary_url_uses_daily_host_for_production() {
        assert_eq!(
            quota_summary_url("https://cloudcode-pa.googleapis.com"),
            "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary"
        );
        assert_eq!(
            quota_summary_url("https://daily-cloudcode-pa.googleapis.com"),
            "https://daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary"
        );
    }

    #[tokio::test]
    async fn fetch_usage_sends_hub_user_agent() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:retrieveUserQuotaSummary"))
            .and(header("user-agent", super::super::version::user_agent()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "groups": [{
                    "displayName": "Gemini Models",
                    "buckets": [{
                        "bucketId": "gemini-5h",
                        "window": "5h",
                        "remainingFraction": 0.7
                    }]
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let buckets = fetch_usage(&reqwest::Client::new(), &server.uri(), "token", "project")
            .await
            .expect("quota-summary fetch succeeds");
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].label, GEMINI_5H_LABEL);
        assert_eq!(buckets[0].remaining, Some(0.7));
    }

    #[tokio::test]
    async fn fetch_usage_errors_on_non_success() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_string("PERMISSION_DENIED"))
            .mount(&server)
            .await;

        let error = fetch_usage(&reqwest::Client::new(), &server.uri(), "token", "project")
            .await
            .expect_err("a 403 must surface as an error");
        assert!(error.to_string().contains("403"), "got: {error}");
    }
}
