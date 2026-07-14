//! Anthropic OAuth usage API client.
//!
//! `GET https://api.anthropic.com/api/oauth/usage` reports a subscription
//! account's authoritative rate-limit utilization across the same windows shunt
//! tracks from the `anthropic-ratelimit-unified-*` response headers — the 5-hour
//! session window, the shared weekly window, and the Fable-scoped weekly window
//! (`7d_oi`). Unlike those headers, the usage API reflects out-of-band
//! consumption of the same account (the operator's own Claude Code, other
//! tools), so the pool polls it to reconcile header-derived state that would
//! otherwise undercount usage — see [`crate::usage_poll`].
//!
//! The endpoint authenticates with an OAuth bearer and only accepts a
//! *refreshable* login token; a long-lived `claude setup-token` is rejected, so
//! the poller restricts itself to imported accounts.

use crate::accounts::{UsageSnapshot, UsageWindow};

/// Path appended to a provider's base URL to reach the usage endpoint.
pub const USAGE_PATH: &str = "/api/oauth/usage";

/// The Anthropic-assigned display name of the Fable-scoped weekly limit in the
/// usage response's `limits[]` array (`kind == "weekly_scoped"`). Mirrors the
/// `7d_oi` unified rate-limit bucket shunt tracks for Fable models.
const FABLE_SCOPE_DISPLAY_NAME: &str = "Fable";

/// Fetch and parse the usage snapshot for one OAuth account. `base_url` is the
/// provider's base (e.g. `https://api.anthropic.com`); `access_token` is a valid
/// refreshable-login bearer.
pub async fn fetch_usage(
    client: &reqwest::Client,
    base_url: &str,
    access_token: &str,
) -> anyhow::Result<UsageSnapshot> {
    let url = format!("{}{USAGE_PATH}", base_url.trim_end_matches('/'));
    let response = client
        .get(&url)
        .header("authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("anthropic-version", "2023-06-01")
        // The shared client carries no default timeout; bound this background poll
        // so a hung connection can never stall the poller task indefinitely.
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail: String = text.chars().take(200).collect();
        anyhow::bail!("usage request failed ({status}): {detail}");
    }
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| anyhow::anyhow!("invalid usage response: {error}"))?;
    Ok(parse_usage(&value))
}

/// Parse the usage JSON into a [`UsageSnapshot`]. Tolerant of missing fields: an
/// absent or malformed window yields `None` rather than an error, so a partial
/// response still reconciles whatever it does report.
fn parse_usage(value: &serde_json::Value) -> UsageSnapshot {
    UsageSnapshot {
        five_hour: parse_window(value.get("five_hour")),
        seven_day: parse_window(value.get("seven_day")),
        seven_day_oi: parse_fable_window(value.get("limits")),
    }
}

/// Parse a top-level `{ "utilization": <0-100>, "resets_at": <rfc3339> }` object.
fn parse_window(value: Option<&serde_json::Value>) -> Option<UsageWindow> {
    let object = value?;
    let utilization = object
        .get("utilization")
        .and_then(serde_json::Value::as_f64)?;
    Some(UsageWindow {
        utilization: normalize_percent(utilization),
        resets_at: object
            .get("resets_at")
            .and_then(serde_json::Value::as_str)
            .and_then(parse_rfc3339_to_epoch_secs),
    })
}

/// Locate the Fable-scoped weekly limit in `limits[]` and map its `percent` +
/// `resets_at` to a window. The array carries the model-scoped limit as
/// `kind == "weekly_scoped"` with `scope.model.display_name == "Fable"`.
fn parse_fable_window(limits: Option<&serde_json::Value>) -> Option<UsageWindow> {
    let entry = limits?.as_array()?.iter().find(|limit| {
        limit.get("kind").and_then(serde_json::Value::as_str) == Some("weekly_scoped")
            && limit
                .pointer("/scope/model/display_name")
                .and_then(serde_json::Value::as_str)
                == Some(FABLE_SCOPE_DISPLAY_NAME)
    })?;
    let percent = entry.get("percent").and_then(serde_json::Value::as_f64)?;
    Some(UsageWindow {
        utilization: normalize_percent(percent),
        resets_at: entry
            .get("resets_at")
            .and_then(serde_json::Value::as_str)
            .and_then(parse_rfc3339_to_epoch_secs),
    })
}

/// Convert an API percent (0–100, occasionally above 100 when over the cap) to
/// the pool's `0.0..` fraction. Negatives are clamped to 0.
fn normalize_percent(percent: f64) -> f64 {
    (percent / 100.0).max(0.0)
}

/// Parse an RFC 3339 timestamp (`2026-07-14T17:30:00.045562+00:00`, or a `Z`
/// suffix) to whole Unix epoch seconds. Fractional seconds are dropped. Returns
/// `None` on any malformed component so a bad timestamp degrades to "no reset"
/// rather than poisoning the whole snapshot. The codebase carries no date crate;
/// this is a focused, self-contained parser for exactly this endpoint's shape.
fn parse_rfc3339_to_epoch_secs(input: &str) -> Option<u64> {
    let (date, rest) = input.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    // Bound the year to a sane 4-digit range: we only ever handle post-epoch
    // timestamps, and this keeps `days_from_civil`'s i64 arithmetic far from
    // overflow on an absurdly large parsed year.
    if date_parts.next().is_some()
        || !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
    {
        return None;
    }

    // Split the offset off the time. Order matters: check `Z` first, then a `+`
    // offset, then a `-` offset — but the fractional-seconds dot never contains
    // those, so scanning for the first offset marker is unambiguous.
    let (clock, offset_secs) = split_offset(rest)?;

    // clock is `HH:MM:SS` optionally followed by `.fraction` (dropped).
    let clock = clock.split('.').next().unwrap_or(clock);
    let mut clock_parts = clock.split(':');
    let hour: i64 = clock_parts.next()?.parse().ok()?;
    let minute: i64 = clock_parts.next()?.parse().ok()?;
    let second: i64 = clock_parts.next()?.parse().ok()?;
    if clock_parts.next().is_some()
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let epoch = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_secs;
    u64::try_from(epoch).ok()
}

/// Separate a time-of-day string from its trailing UTC offset, returning the
/// clock portion and the offset in seconds (to *subtract* to reach UTC).
fn split_offset(rest: &str) -> Option<(&str, i64)> {
    if let Some(clock) = rest.strip_suffix('Z').or_else(|| rest.strip_suffix('z')) {
        return Some((clock, 0));
    }
    // Find the sign that begins the offset. It is the last '+' or '-' in the
    // string (the clock itself has no sign), so scan from the right.
    let sign_pos = rest.rfind(['+', '-'])?;
    let (clock, offset) = rest.split_at(sign_pos);
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    let mut offset_parts = offset[1..].split(':');
    let offset_hour: i64 = offset_parts.next()?.parse().ok()?;
    let offset_minute: i64 = offset_parts.next().unwrap_or("0").parse().ok()?;
    if offset_parts.next().is_some()
        || !(0..=23).contains(&offset_hour)
        || !(0..=59).contains(&offset_minute)
    {
        return None;
    }
    Some((clock, sign * (offset_hour * 3_600 + offset_minute * 60)))
}

/// Days from the Unix epoch (1970-01-01) to the given civil date, by Howard
/// Hinnant's `days_from_civil` algorithm. Valid for the proleptic Gregorian
/// calendar; negative for dates before the epoch.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400; // [0, 399]
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_rfc3339_timestamps() {
        // Reference epochs computed independently (UTC).
        assert_eq!(parse_rfc3339_to_epoch_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_to_epoch_secs("2021-01-01T00:00:00Z"),
            Some(1_609_459_200)
        );
        // Fractional seconds are dropped; `+00:00` offset is UTC.
        assert_eq!(
            parse_rfc3339_to_epoch_secs("2021-01-01T00:00:00.123456+00:00"),
            Some(1_609_459_200)
        );
        // A positive offset is subtracted to reach UTC: 09:00+09:00 == 00:00Z.
        assert_eq!(
            parse_rfc3339_to_epoch_secs("2021-01-01T09:00:00+09:00"),
            Some(1_609_459_200)
        );
        // A negative offset is added back: 19:00-05:00 (prev day) == 00:00Z.
        assert_eq!(
            parse_rfc3339_to_epoch_secs("2020-12-31T19:00:00-05:00"),
            Some(1_609_459_200)
        );
    }

    #[test]
    fn rejects_malformed_timestamps() {
        for bad in [
            "not-a-date",
            "2021-01-01",                // no time
            "2021-13-01T00:00:00Z",      // month out of range
            "2021-01-01T25:00:00Z",      // hour out of range
            "2021-01-01T00:00:00+99:00", // offset hour out of range
            "2021-01-01T00:00:00+00:99", // offset minute out of range
            "10000-01-01T00:00:00Z",     // year above the 4-digit range
            "1969-01-01T00:00:00Z",      // before the epoch -> negative -> None on u64
        ] {
            assert_eq!(parse_rfc3339_to_epoch_secs(bad), None, "accepted {bad:?}");
        }
    }

    #[test]
    fn parses_full_usage_payload() {
        let value = serde_json::json!({
            "five_hour": {
                "utilization": 33.0,
                "resets_at": "2026-07-14T17:30:00.045562+00:00"
            },
            "seven_day": {
                "utilization": 88.0,
                "resets_at": "2026-07-19T12:00:00.045584+00:00"
            },
            "limits": [
                { "kind": "session", "percent": 33 },
                { "kind": "weekly_all", "percent": 88 },
                {
                    "kind": "weekly_scoped",
                    "percent": 54,
                    "resets_at": "2026-07-19T12:00:00.045944+00:00",
                    "scope": { "model": { "display_name": "Fable" } }
                }
            ]
        });
        let snapshot = parse_usage(&value);
        let five = snapshot.five_hour.expect("five_hour present");
        assert!((five.utilization - 0.33).abs() < 1e-9);
        assert!(five.resets_at.is_some());
        let seven = snapshot.seven_day.expect("seven_day present");
        assert!((seven.utilization - 0.88).abs() < 1e-9);
        let fable = snapshot.seven_day_oi.expect("fable weekly_scoped present");
        assert!((fable.utilization - 0.54).abs() < 1e-9);
        assert!(fable.resets_at.is_some());
    }

    #[test]
    fn omits_windows_absent_from_payload() {
        // No `five_hour`, no Fable-scoped limit -> those windows are None.
        let value = serde_json::json!({
            "seven_day": { "utilization": 10.0 },
            "limits": [ { "kind": "weekly_all", "percent": 10 } ]
        });
        let snapshot = parse_usage(&value);
        assert!(snapshot.five_hour.is_none());
        assert!(snapshot.seven_day_oi.is_none());
        let seven = snapshot.seven_day.expect("seven_day present");
        assert!((seven.utilization - 0.10).abs() < 1e-9);
        // A window with no resets_at is still reported (utilization only).
        assert!(seven.resets_at.is_none());
    }

    #[tokio::test]
    async fn fetch_usage_parses_success_response() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Split the scheme from the token so the matcher carries no contiguous
        // `Bearer <token>` literal (a credential-scanner false positive).
        let token = "imported-token";
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/oauth/usage"))
            .and(header("authorization", format!("Bearer {token}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "five_hour": { "utilization": 50.0, "resets_at": "2026-07-14T17:30:00+00:00" },
                "seven_day": { "utilization": 60.0 },
                "limits": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let snapshot = fetch_usage(&reqwest::Client::new(), &server.uri(), token)
            .await
            .expect("usage fetch succeeds");
        assert!((snapshot.five_hour.unwrap().utilization - 0.5).abs() < 1e-9);
        assert!((snapshot.seven_day.unwrap().utilization - 0.6).abs() < 1e-9);
        assert!(snapshot.seven_day_oi.is_none());
    }

    #[tokio::test]
    async fn fetch_usage_errors_on_non_success() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .mount(&server)
            .await;

        let error = fetch_usage(&reqwest::Client::new(), &server.uri(), "bad-token")
            .await
            .expect_err("a 401 must surface as an error");
        assert!(error.to_string().contains("401"), "got: {error}");
    }
}
