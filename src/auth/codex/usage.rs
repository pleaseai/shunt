//! ChatGPT/Codex `wham/usage` client.
//!
//! `GET {chatgpt_base_url}/wham/usage` reports a ChatGPT/Codex subscription
//! account's rate-limit utilization for the same 5-hour and weekly windows
//! shunt tracks from the proxied `x-codex-*` response headers (see
//! [`crate::accounts::AccountPool::note_codex_quota`]). Those headers only
//! update on traffic that actually flowed through shunt, so an account the
//! pool has excluded for being near quota never gets a fresh observation —
//! even after the upstream window has long since reset. Codex CLI itself
//! polls this endpoint every 60 seconds for the same reason: to know an
//! account's headroom without waiting on live traffic. The poller
//! ([`crate::usage_poll`]) reuses it to reconcile header-derived state the
//! same way [`crate::auth::claude::usage`] does for Claude.
//!
//! **This is an unofficial, private API.** It is not part of any published
//! OpenAI/ChatGPT contract — the evidence for its shape is the Codex CLI's
//! own observed polling behavior, not documentation. Its schema can drift or
//! disappear without notice, so parsing here is deliberately lenient and
//! fail-soft end to end: an unrecognized container or a response with no
//! identifiable window returns `Err` (and the poller only logs it at debug),
//! a single bad window is skipped rather than failing the whole response, and
//! no code path here can panic or mark an account unhealthy on a parse
//! failure. Losing this signal degrades the pool back to header-only
//! tracking; it must never take a proxied request down with it.
//!
//! The parser buckets windows by their reported duration, never by a key's
//! position. An unrecognized duration is skipped. The upstream policy
//! currently observed in real accounts can return only a weekly window in
//! primary_window with secondary_window: null, but that is an observed state
//! rather than an API contract. If a 5-hour window returns in either key, the
//! duration-based parser handles it without a code change.
//!
//! The endpoint authenticates with the same ChatGPT OAuth bearer and CLI
//! identity headers as the Responses API's ChatGPT backend, plus the
//! `chatgpt-account-id` header — see [`fetch_usage`]. Like the Claude usage
//! API, only a refreshable imported login can call it; the poller restricts
//! itself to imported Codex accounts.

use anyhow::Context;
use serde_json::Value;

use crate::accounts::{codex_window_bucket, CodexWindow, UsageSnapshot, UsageWindow};
use crate::adapters::responses::request::{CODEX_CLIENT_VERSION, CODEX_USER_AGENT};

/// Path appended to a provider's base URL to reach the usage endpoint.
pub const USAGE_PATH: &str = "/wham/usage";

/// Fetch and parse the wham usage snapshot for one ChatGPT/Codex OAuth
/// account. `base_url` is the provider's ChatGPT backend base (e.g.
/// `https://chatgpt.com/backend-api`); `access_token` is a valid refreshable
/// ChatGPT login bearer and `account_id` is that account's ChatGPT account
/// id, exactly as sent on `/codex/responses`
/// (`adapters::responses::request::build_request`).
pub async fn fetch_usage(
    client: &reqwest::Client,
    base_url: &str,
    access_token: &str,
    account_id: &str,
) -> anyhow::Result<UsageSnapshot> {
    Ok(
        fetch_usage_report(client, base_url, access_token, account_id)
            .await?
            .usage,
    )
}

/// Fetch the wham usage snapshot together with Codex-only authoritative
/// absence decisions for the account-pool poller.
pub(crate) async fn fetch_usage_report(
    client: &reqwest::Client,
    base_url: &str,
    access_token: &str,
    account_id: &str,
) -> anyhow::Result<WhamUsageReport> {
    let url = format!("{}{USAGE_PATH}", base_url.trim_end_matches('/'));
    let response = client
        .get(&url)
        .header("authorization", format!("Bearer {access_token}"))
        .header("chatgpt-account-id", account_id)
        .header("originator", "codex_cli_rs")
        .header("user-agent", CODEX_USER_AGENT)
        .header("version", CODEX_CLIENT_VERSION)
        // The shared client carries no default timeout; bound this background poll
        // so a hung connection can never stall the poller task indefinitely.
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?;
    let status = response.status();
    let text = response
        .text()
        .await
        .context("Codex wham usage response body read failed")?;
    if !status.is_success() {
        let detail: String = text.chars().take(200).collect();
        anyhow::bail!("wham usage request failed ({status}): {detail}");
    }
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| anyhow::anyhow!("invalid wham usage response: {error}"))?;
    parse_usage(&value)
}

/// A parsed wham report plus the buckets whose utilization the response
/// authoritatively omitted. Unlike Claude's usage API, wham enumerates the
/// account's 5h/7d windows, so a missing bucket can clear stale header-derived
/// utilization. Reset and status metadata remain header-derived. An
/// unknown-duration window suppresses both clear decisions because its bucket
/// cannot be inferred safely.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WhamUsageReport {
    pub(crate) usage: UsageSnapshot,
    pub(crate) clear_five_hour: bool,
    pub(crate) clear_seven_day: bool,
}

/// Parse the wham usage JSON into a [`WhamUsageReport`]. Every non-null value in
/// primary_window, secondary_window, five_hour_limit, and weekly_limit is
/// considered independently. A window is bucketed only from its reported
/// duration, using limit_window_seconds first, then window_minutes, then
/// limit_window_minutes; an unknown or malformed duration skips that window
/// without a positional fallback. A returned 5-hour window therefore works in
/// either key without code changes, while the currently observed upstream
/// policy may return only a weekly primary window with a null secondary value.
/// The latter is an observation, not an API contract. A structurally
/// recognizable response with no usable window still returns an empty snapshot;
/// a response with no recognized window keys at all returns Err so an
/// unrelated payload is not trusted.
fn parse_usage(value: &serde_json::Value) -> anyhow::Result<WhamUsageReport> {
    let container = value
        .get("rate_limit")
        .or_else(|| value.get("rate_limits"))
        .unwrap_or(value);
    if !container.is_object() {
        anyhow::bail!("wham usage response is not a JSON object");
    }
    let windows = [
        container.get("primary_window"),
        container.get("secondary_window"),
        container.get("five_hour_limit"),
        container.get("weekly_limit"),
    ];
    if windows.iter().all(Option::is_none) {
        anyhow::bail!("wham usage response carries no recognizable rate-limit window");
    }

    let mut five_hour = None;
    let mut seven_day = None;
    let mut has_five_hour_candidate = false;
    let mut has_seven_day_candidate = false;
    let mut has_unknown_duration_candidate = false;
    for window in windows.into_iter().flatten() {
        if window.is_null() {
            continue;
        }
        let Some(bucket) = window_bucket(window) else {
            has_unknown_duration_candidate = true;
            continue;
        };
        match bucket {
            CodexWindow::FiveHour => has_five_hour_candidate = true,
            CodexWindow::Weekly => has_seven_day_candidate = true,
        }
        let Some(parsed) = parse_window(window) else {
            continue;
        };
        match bucket {
            CodexWindow::FiveHour if five_hour.is_none() => five_hour = Some(parsed),
            CodexWindow::Weekly if seven_day.is_none() => seven_day = Some(parsed),
            CodexWindow::FiveHour | CodexWindow::Weekly => {
                tracing::debug!(?bucket, "dropping duplicate wham usage window bucket");
            }
        }
    }

    // The Fable-scoped weekly bucket (`7d_oi`) has no wham/usage equivalent:
    // that limit is an Anthropic/Claude concept the ChatGPT backend does not
    // report. Always `None` here; the Codex-only consumer never clears it.
    Ok(WhamUsageReport {
        usage: UsageSnapshot {
            five_hour,
            seven_day,
            seven_day_oi: None,
        },
        clear_five_hour: !has_unknown_duration_candidate && !has_five_hour_candidate,
        clear_seven_day: !has_unknown_duration_candidate && !has_seven_day_candidate,
    })
}

/// Parse one window object: `{ "used_percent": <0-100>, "reset_at": ... }`.
/// `reset_at` accepts an unsigned epoch or RFC3339 timestamp. `None` on any
/// problem with this window alone (missing/non-finite/out-of-range percent)
/// means the caller skips it and still applies whatever else the response
/// reported.
fn parse_window(value: &serde_json::Value) -> Option<UsageWindow> {
    let percent = value.get("used_percent").and_then(Value::as_f64)?;
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        return None;
    }
    Some(UsageWindow {
        utilization: percent / 100.0,
        resets_at: value.get("reset_at").and_then(parse_reset_at),
    })
}

/// Parse the private endpoint's reset instant without inferring unsupported
/// encodings. JSON integers are accepted only as non-negative epochs; numeric
/// strings, floats, negative values, and other values remain absent signals.
fn parse_reset_at(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_str()
            .and_then(crate::auth::claude::usage::parse_rfc3339_to_epoch_secs)
    })
}

/// Classify a window from its reported duration. Field presence is strict:
/// when a higher-priority duration field exists but is malformed, negative, or
/// outside a known bucket, lower-priority fields are not consulted.
fn window_bucket(value: &serde_json::Value) -> Option<CodexWindow> {
    let minutes = if let Some(seconds) = value.get("limit_window_seconds") {
        let seconds = seconds.as_i64()?;
        if seconds < 0 {
            return None;
        }
        seconds / 60
    } else if let Some(minutes) = value.get("window_minutes") {
        let minutes = minutes.as_i64()?;
        if minutes < 0 {
            return None;
        }
        minutes
    } else {
        let minutes = value.get("limit_window_minutes")?.as_i64()?;
        if minutes < 0 {
            return None;
        }
        minutes
    };
    codex_window_bucket(minutes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_current_weekly_primary_shape() {
        let value = serde_json::json!({
            "rate_limit": {
                "primary_window": {
                    "used_percent": 100,
                    "limit_window_seconds": 604_800,
                    "reset_after_seconds": 493_436,
                    "reset_at": 1_788_142_293u64
                },
                "secondary_window": null
            }
        });
        let snapshot = parse_usage(&value)
            .expect("live-shaped response is recognizable")
            .usage;
        assert!(snapshot.five_hour.is_none());
        let weekly = snapshot
            .seven_day
            .as_ref()
            .expect("weekly duration is classified");
        assert_eq!(weekly.utilization, 1.0);
        assert_eq!(weekly.resets_at, Some(1_788_142_293));
        assert!(!snapshot.is_empty());
    }

    #[test]
    fn skips_window_without_recognized_duration() {
        let value = serde_json::json!({
            "primary_window": {
                "used_percent": 42,
                "reset_at": 1_788_142_293u64
            },
            "secondary_window": null
        });
        let snapshot = parse_usage(&value)
            .expect("window keys are recognizable")
            .usage;
        assert!(snapshot.is_empty());
    }

    #[test]
    fn classifies_five_hour_primary_and_weekly_secondary_by_duration() {
        let value = serde_json::json!({
            "primary_window": {
                "used_percent": 20,
                "limit_window_seconds": 18_000,
                "reset_at": 1_700_000_001u64
            },
            "secondary_window": {
                "used_percent": 80,
                "limit_window_seconds": 604_800,
                "reset_at": 1_700_000_002u64
            }
        });
        let snapshot = parse_usage(&value)
            .expect("both windows are recognizable")
            .usage;
        assert_eq!(snapshot.five_hour.unwrap().utilization, 0.2);
        assert_eq!(snapshot.seven_day.unwrap().utilization, 0.8);
    }

    #[test]
    fn classifies_weekly_primary_and_five_hour_secondary_by_duration() {
        let value = serde_json::json!({
            "primary_window": {
                "used_percent": 80,
                "limit_window_seconds": 604_800,
                "reset_at": 1_700_000_003u64
            },
            "secondary_window": {
                "used_percent": 20,
                "limit_window_seconds": 18_000,
                "reset_at": 1_700_000_004u64
            }
        });
        let snapshot = parse_usage(&value)
            .expect("both windows are recognizable")
            .usage;
        assert_eq!(snapshot.five_hour.unwrap().utilization, 0.2);
        assert_eq!(snapshot.seven_day.unwrap().utilization, 0.8);
    }

    #[test]
    fn accepts_plausible_duration_drift() {
        let value = serde_json::json!({
            "primary_window": {
                "used_percent": 20,
                "limit_window_seconds": 18_360
            },
            "secondary_window": {
                "used_percent": 80,
                "limit_window_seconds": 630_000
            }
        });
        let snapshot = parse_usage(&value)
            .expect("drifted durations remain recognizable")
            .usage;
        assert!(snapshot.five_hour.is_some());
        assert!(snapshot.seven_day.is_some());
    }

    #[test]
    fn duration_field_precedence_is_strict() {
        let cases = [
            (
                serde_json::json!({
                    "primary_window": {
                        "used_percent": 10,
                        "limit_window_seconds": 18_000,
                        "window_minutes": 10_080,
                        "limit_window_minutes": 10_080
                    }
                }),
                true,
                false,
            ),
            (
                serde_json::json!({
                    "primary_window": {
                        "used_percent": 10,
                        "limit_window_seconds": "18_000",
                        "window_minutes": 300
                    }
                }),
                false,
                false,
            ),
            (
                serde_json::json!({
                    "primary_window": {
                        "used_percent": 10,
                        "limit_window_seconds": 18_000.5,
                        "window_minutes": 300
                    }
                }),
                false,
                false,
            ),
            (
                serde_json::json!({
                    "primary_window": {
                        "used_percent": 10,
                        "limit_window_seconds": -18_000,
                        "window_minutes": 300
                    }
                }),
                false,
                false,
            ),
            (
                serde_json::json!({
                    "primary_window": {
                        "used_percent": 10,
                        "limit_window_seconds": 42,
                        "window_minutes": 300
                    }
                }),
                false,
                false,
            ),
            (
                serde_json::json!({
                    "primary_window": {
                        "used_percent": 10,
                        "window_minutes": "300",
                        "limit_window_minutes": 10_080
                    }
                }),
                false,
                false,
            ),
            (
                serde_json::json!({
                    "primary_window": {
                        "used_percent": 10,
                        "window_minutes": -300,
                        "limit_window_minutes": 300
                    }
                }),
                false,
                false,
            ),
            (
                serde_json::json!({
                    "primary_window": {
                        "used_percent": 10,
                        "limit_window_minutes": -300
                    }
                }),
                false,
                false,
            ),
        ];

        for (value, expect_five_hour, expect_weekly) in cases {
            let snapshot = parse_usage(&value)
                .expect("window key is recognizable")
                .usage;
            assert_eq!(snapshot.five_hour.is_some(), expect_five_hour);
            assert_eq!(snapshot.seven_day.is_some(), expect_weekly);
        }
    }

    #[test]
    fn supports_all_duration_field_names() {
        let cases = [
            ("limit_window_seconds", serde_json::json!(18_000), true),
            ("window_minutes", serde_json::json!(10_080), false),
            ("limit_window_minutes", serde_json::json!(300), true),
        ];
        for (field, duration, expect_five_hour) in cases {
            let mut window = serde_json::Map::new();
            window.insert("used_percent".to_owned(), serde_json::json!(25));
            window.insert(field.to_owned(), duration);
            let value = serde_json::json!({ "primary_window": window });
            let snapshot = parse_usage(&value)
                .expect("duration field is recognized")
                .usage;
            assert_eq!(snapshot.five_hour.is_some(), expect_five_hour);
            assert_eq!(snapshot.seven_day.is_some(), !expect_five_hour);
        }
    }

    #[test]
    fn applies_valid_alias_after_invalid_primary() {
        let value = serde_json::json!({
            "primary_window": {
                "used_percent": 10,
                "limit_window_seconds": "invalid"
            },
            "five_hour_limit": {
                "used_percent": 60,
                "window_minutes": 300
            }
        });
        let snapshot = parse_usage(&value).expect("recognized window keys").usage;
        assert_eq!(snapshot.five_hour.unwrap().utilization, 0.6);
    }

    #[test]
    fn retains_first_valid_window_per_bucket() {
        let value = serde_json::json!({
            "primary_window": {
                "used_percent": 10,
                "window_minutes": 300
            },
            "secondary_window": {
                "used_percent": 90,
                "window_minutes": 300
            },
            "weekly_limit": {
                "used_percent": 70,
                "limit_window_minutes": 10_080
            }
        });
        let snapshot = parse_usage(&value).expect("recognized window keys").usage;
        assert_eq!(snapshot.five_hour.unwrap().utilization, 0.1);
        assert_eq!(snapshot.seven_day.unwrap().utilization, 0.7);
    }

    #[test]
    fn accepts_unsigned_or_rfc3339_reset_at() {
        let cases = [
            (serde_json::json!(1_700_000_000u64), Some(1_700_000_000)),
            (
                serde_json::json!("2021-01-01T00:00:00Z"),
                Some(1_609_459_200),
            ),
            (
                serde_json::json!("2021-01-01T09:00:00+09:00"),
                Some(1_609_459_200),
            ),
        ];

        for (reset_at, expected) in cases {
            let value = serde_json::json!({
                "primary_window": {
                    "used_percent": 10,
                    "window_minutes": 300,
                    "reset_at": reset_at
                }
            });
            let snapshot = parse_usage(&value).expect("window is recognizable").usage;
            let parsed = snapshot.five_hour.expect("duration and utilization remain");
            assert_eq!(parsed.resets_at, expected);
        }
    }

    #[test]
    fn rejects_unsupported_reset_at_encodings() {
        let cases = [
            serde_json::json!("1700000000"),
            serde_json::json!(1_700_000_000.5),
            serde_json::json!(-1),
            serde_json::json!("not-a-timestamp"),
            serde_json::json!(true),
            serde_json::json!(null),
        ];

        for reset_at in cases {
            let value = serde_json::json!({
                "primary_window": {
                    "used_percent": 10,
                    "window_minutes": 300,
                    "reset_at": reset_at
                }
            });
            let snapshot = parse_usage(&value).expect("window is recognizable").usage;
            let parsed = snapshot.five_hour.expect("duration and utilization remain");
            assert!(parsed.resets_at.is_none(), "accepted {reset_at:?}");
        }
    }

    #[test]
    fn rejects_out_of_range_or_non_finite_percent() {
        for bad in [-1.0, 100.5, f64::NAN, f64::INFINITY] {
            let value = serde_json::json!({
                "primary_window": {
                    "used_percent": bad,
                    "window_minutes": 300
                }
            });
            let snapshot = parse_usage(&value)
                .expect("primary window key is recognized")
                .usage;
            assert!(
                snapshot.five_hour.is_none(),
                "percent {bad} should be rejected"
            );
        }
    }

    #[test]
    fn ignores_additional_rate_limits() {
        let value = serde_json::json!({
            "primary_window": {
                "used_percent": 5.0,
                "window_minutes": 300
            },
            "additional_rate_limits": [
                { "kind": "unmodeled", "used_percent": 99.0, "window_minutes": 10_080 }
            ]
        });
        let snapshot = parse_usage(&value).expect("recognizable window").usage;
        assert_eq!(snapshot.five_hour.unwrap().utilization, 0.05);
        assert!(snapshot.seven_day.is_none());
    }

    #[test]
    fn unrecognizable_response_shape_is_an_error() {
        for value in [
            serde_json::json!({ "totally_unexpected": true }),
            serde_json::json!([1, 2, 3]),
            serde_json::json!("just a string"),
            serde_json::json!({ "rate_limit": "not an object" }),
        ] {
            assert!(
                parse_usage(&value).is_err(),
                "expected an error for {value:?}"
            );
        }
    }

    #[test]
    fn tolerates_alternate_field_names() {
        let value = serde_json::json!({
            "rate_limits": {
                "five_hour_limit": {
                    "used_percent": 25.0,
                    "limit_window_minutes": 300,
                    "reset_at": 1_700_000_005u64
                },
                "weekly_limit": {
                    "used_percent": 75.0,
                    "limit_window_seconds": 604_800,
                    "reset_at": 1_700_000_006u64
                }
            }
        });
        let snapshot = parse_usage(&value)
            .expect("alternate names recognized")
            .usage;
        assert_eq!(snapshot.five_hour.unwrap().utilization, 0.25);
        assert_eq!(snapshot.seven_day.unwrap().utilization, 0.75);
    }

    #[tokio::test]
    async fn fetch_usage_applies_wham_snapshot_and_sends_identity_headers() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let token = "imported-chatgpt-token";
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/wham/usage"))
            .and(header("authorization", format!("Bearer {token}")))
            .and(header("chatgpt-account-id", "acct-123"))
            .and(header("originator", "codex_cli_rs"))
            .and(header("user-agent", CODEX_USER_AGENT))
            .and(header("version", CODEX_CLIENT_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "rate_limit": {
                    "primary_window": {
                        "used_percent": 33.0,
                        "limit_window_seconds": 18_000,
                        "reset_at": 1_700_000_007u64
                    },
                    "secondary_window": {
                        "used_percent": 88.0,
                        "limit_window_seconds": 604_800,
                        "reset_at": 1_700_000_008u64
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let snapshot = fetch_usage(&reqwest::Client::new(), &server.uri(), token, "acct-123")
            .await
            .expect("usage fetch succeeds");
        let five_hour = snapshot.five_hour.expect("primary_window applied");
        assert_eq!(five_hour.utilization, 0.33);
        assert_eq!(five_hour.resets_at, Some(1_700_000_007));
        let seven_day = snapshot.seven_day.expect("secondary_window applied");
        assert_eq!(seven_day.utilization, 0.88);
        assert_eq!(seven_day.resets_at, Some(1_700_000_008));
        assert!(snapshot.seven_day_oi.is_none());
    }

    #[tokio::test]
    async fn fetch_usage_errors_on_non_success() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let error = fetch_usage(&reqwest::Client::new(), &server.uri(), "bad-token", "acct")
            .await
            .expect_err("a 500 must surface as an error");
        assert!(error.to_string().contains("500"), "got: {error}");
    }

    #[tokio::test]
    async fn fetch_usage_errors_on_unrecognizable_body() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "totally_unexpected": true })),
            )
            .mount(&server)
            .await;

        let error = fetch_usage(&reqwest::Client::new(), &server.uri(), "token", "acct")
            .await
            .expect_err("an unrecognizable 200 body must still surface as an error");
        assert!(error.to_string().contains("wham"), "got: {error}");
    }
}
