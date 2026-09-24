//! Unit tests for the conditional router's rule walk and the pure predicates.
//!
//! `TimeBetween::contains` and `HeaderMatch::matches` are pure and tested
//! directly. The request-aware rule walk goes through `select_at`, which takes
//! the instant explicitly, so every time-window assertion below is
//! deterministic — it does not read the real clock and so cannot pass or fail
//! by the wall time the suite happened to run at.

use time::{Date, Month, OffsetDateTime, Time};

use crate::config::router::conditional::{
    ConditionalRouterConfig, ConditionalRule, HeaderMatch, HeaderMatchMode, TimeBetween, Weekday,
    WhenCondition,
};
use crate::routing::conditional::select_at;
use crate::routing::outcome::RouteSource;

/// A fixed instant for every test: 2026-09-24 12:00:00 UTC, a Thursday.
fn noon_utc() -> OffsetDateTime {
    Date::from_calendar_date(2026, Month::September, 24)
        .unwrap()
        .with_hms(12, 0, 0)
        .unwrap()
        .assume_utc()
}

/// The same fixed day at `hour:minute` UTC.
fn at_utc(hour: u8, minute: u8) -> OffsetDateTime {
    Date::from_calendar_date(2026, Month::September, 24)
        .unwrap()
        .with_time(Time::from_hms(hour, minute, 0).unwrap())
        .assume_utc()
}

fn time_between(start: &str, end: &str) -> TimeBetween {
    let (sh, sm) = crate::config::parse_hh_mm(start).unwrap();
    let (eh, em) = crate::config::parse_hh_mm(end).unwrap();
    TimeBetween {
        start: (sh, sm),
        end: (eh, em),
    }
}

fn rule(name: &str, priority: u32, target: &str, when: WhenCondition) -> ConditionalRule {
    ConditionalRule {
        name: name.into(),
        priority,
        target: target.into(),
        when,
    }
}

fn config(
    rules: Vec<ConditionalRule>,
    default_target: &str,
    utc_offset_hours: Option<f64>,
) -> ConditionalRouterConfig {
    ConditionalRouterConfig {
        utc_offset_hours,
        rules,
        default_target: default_target.into(),
    }
}

/// Midnight-crossing window: 22:00 -> 08:00.
#[test]
fn midnight_crossing_window_boundaries() {
    let w = time_between("22:00", "08:00");
    assert!(w.contains(22, 0), "start is inclusive");
    assert!(w.contains(23, 59), "just before midnight");
    assert!(w.contains(0, 0), "midnight");
    assert!(w.contains(7, 59), "just before end");
    assert!(!w.contains(8, 0), "end is exclusive");
    assert!(!w.contains(21, 59), "just before start");
    assert!(!w.contains(12, 0), "midday is outside");
}

/// Same-day window: 08:00 -> 22:00.
#[test]
fn same_day_window_boundaries() {
    let w = time_between("08:00", "22:00");
    assert!(w.contains(8, 0));
    assert!(w.contains(21, 59));
    assert!(!w.contains(22, 0));
    assert!(!w.contains(7, 59));
    assert!(!w.contains(0, 0));
}

/// Header matching is ASCII case-insensitive in every mode.
#[test]
fn header_match_modes() {
    let eq = HeaderMatch {
        name: "h".into(),
        value: "abc".into(),
        mode: HeaderMatchMode::Equals,
    };
    assert!(eq.matches("abc"));
    assert!(eq.matches("ABC"));
    assert!(!eq.matches("abcd"));

    let contains = HeaderMatch {
        name: "h".into(),
        value: "bcd".into(),
        mode: HeaderMatchMode::Contains,
    };
    assert!(contains.matches("abcdef"));
    assert!(!contains.matches("axcxef"));

    let starts = HeaderMatch {
        name: "h".into(),
        value: "abc".into(),
        mode: HeaderMatchMode::StartsWith,
    };
    assert!(starts.matches("abcdef"));
    assert!(!starts.matches("xabcdef"));
    assert!(!starts.matches("ab"), "longer needle cannot be a prefix");
    // Compare on the byte slice without allocating; this input makes the naive
    // `actual[..value.len()]` form panic.
    assert!(!starts.matches("éab"), "respect UTF-8 char boundaries");
}

/// A header-gated rule never matches a body-less resolution (no headers), and
/// falls through to `default_target` — at a fixed instant, so this cannot be
/// perturbed by the real clock.
#[test]
fn header_rule_falls_through_without_a_request() {
    let cfg = config(
        vec![rule(
            "vip",
            1,
            "glm-4-flash",
            WhenCondition {
                header: Some(HeaderMatch {
                    name: "x-tier".into(),
                    value: "vip".into(),
                    mode: HeaderMatchMode::Equals,
                }),
                ..Default::default()
            },
        )],
        "deepseek-chat",
        Some(0.0),
    );

    let (target, source) = select_at(&cfg, "claude-sonnet", None, noon_utc());
    assert_eq!(target, "deepseek-chat");
    assert_eq!(source, RouteSource::ConditionalDefault);
}

/// A header rule reads `HeaderMap` by name, including when the operator writes
/// the configured name in a different ASCII case than the wire's canonical form.
#[test]
fn header_rule_matches_a_live_request_header_case_insensitively() {
    let cfg = config(
        vec![rule(
            "vip",
            1,
            "glm-4-flash",
            WhenCondition {
                header: Some(HeaderMatch {
                    name: "X-Shunt-Tier".into(),
                    value: "vip".into(),
                    mode: HeaderMatchMode::Equals,
                }),
                ..Default::default()
            },
        )],
        "deepseek-chat",
        Some(0.0),
    );
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::HeaderName::from_static("x-shunt-tier"),
        axum::http::HeaderValue::from_static("VIP"),
    );

    let (target, source) = select_at(&cfg, "claude-sonnet", Some(&headers), noon_utc());
    assert_eq!(target, "glm-4-flash");
    assert_eq!(source, RouteSource::Conditional);
}

/// A model-prefix rule matches on a live model id and routes to its target.
#[test]
fn model_prefix_rule_matches() {
    let cfg = config(
        vec![rule(
            "sonnet-to-cheap",
            1,
            "glm-4-flash",
            WhenCondition {
                model_starts_with: Some("claude-sonnet".into()),
                ..Default::default()
            },
        )],
        "deepseek-chat",
        None,
    );

    let (target, source) = select_at(&cfg, "claude-sonnet-4-5-20260514", None, noon_utc());
    assert_eq!(target, "glm-4-flash");
    assert_eq!(source, RouteSource::Conditional);
}

/// The first rule in priority order wins, not the first declared.
#[test]
fn priority_order_decides() {
    let cfg = config(
        vec![
            rule(
                "low",
                10,
                "glm-4-flash",
                WhenCondition {
                    model_starts_with: Some("claude".into()),
                    ..Default::default()
                },
            ),
            rule(
                "high",
                1,
                "deepseek-chat",
                WhenCondition {
                    model_starts_with: Some("claude".into()),
                    ..Default::default()
                },
            ),
        ],
        "fallback",
        None,
    );

    // `select_at` walks the list as given; the loader sorts by priority. Pass a
    // sorted list to prove the winner.
    let mut sorted = cfg.clone();
    sorted.rules.sort_by_key(|r| r.priority);
    let (target, _) = select_at(&sorted, "claude-opus", None, noon_utc());
    assert_eq!(target, "deepseek-chat", "the lower priority number wins");
}

/// An off-peak window routes at a fixed 23:30 instant and falls through at
/// 12:00 — both deterministic.
#[test]
fn offpeak_window_selects_by_clock() {
    let cfg = config(
        vec![rule(
            "offpeak",
            1,
            "deepseek-chat",
            WhenCondition {
                time_between: Some(time_between("22:00", "08:00")),
                ..Default::default()
            },
        )],
        "glm-4-flash",
        Some(0.0),
    );

    let (target, source) = select_at(&cfg, "claude-sonnet", None, at_utc(23, 30));
    assert_eq!(target, "deepseek-chat");
    assert_eq!(source, RouteSource::Conditional);

    let (target, source) = select_at(&cfg, "claude-sonnet", None, at_utc(12, 0));
    assert_eq!(target, "glm-4-flash", "midday is not off-peak");
    assert_eq!(source, RouteSource::ConditionalDefault);
}

/// A negative fractional offset shifts the clock backwards, not forwards. UTC
/// 04:00 with `-0.5` is 03:30 local, so an off-peak window ending at 04:00 still
/// matches; if the sign were lost it would read 04:30 and miss.
#[test]
fn negative_fractional_offset_shifts_backwards() {
    let cfg = config(
        vec![rule(
            "late",
            1,
            "deepseek-chat",
            WhenCondition {
                time_between: Some(time_between("22:00", "04:00")),
                ..Default::default()
            },
        )],
        "glm-4-flash",
        Some(-0.5),
    );

    // 04:00 UTC -> 03:30 at -00:30, inside the window.
    let (target, _) = select_at(&cfg, "claude-sonnet", None, at_utc(4, 0));
    assert_eq!(target, "deepseek-chat");
}

/// A day filter matches only its listed days, tested at a known weekday.
#[test]
fn day_filter_matches_listed_day() {
    // 2026-09-24 is a Thursday.
    let cfg = config(
        vec![rule(
            "thursdays",
            1,
            "glm-4-flash",
            WhenCondition {
                days: Some(vec![Weekday::Thu]),
                ..Default::default()
            },
        )],
        "deepseek-chat",
        Some(0.0),
    );

    let (target, source) = select_at(&cfg, "claude-sonnet", None, noon_utc());
    assert_eq!(target, "glm-4-flash");
    assert_eq!(source, RouteSource::Conditional);

    // The next day is a Friday, which the rule does not list.
    let friday = at_utc(12, 0) + time::Duration::days(1);
    let (target, source) = select_at(&cfg, "claude-sonnet", None, friday);
    assert_eq!(target, "deepseek-chat");
    assert_eq!(source, RouteSource::ConditionalDefault);
}

/// Combined conditions: a time window AND a model prefix must both hold.
#[test]
fn combined_time_and_model_conditions() {
    let cfg = config(
        vec![rule(
            "combo",
            1,
            "glm-4-flash",
            WhenCondition {
                time_between: Some(time_between("00:00", "23:59")),
                model_starts_with: Some("claude-".into()),
                ..Default::default()
            },
        )],
        "deepseek-chat",
        Some(0.0),
    );

    // Noon is inside 00:00-23:59 and the model matches -> match.
    let (target, _) = select_at(&cfg, "claude-sonnet", None, noon_utc());
    assert_eq!(target, "glm-4-flash");

    // Model does not match -> fall through even though the clock does.
    let (target, source) = select_at(&cfg, "gpt-5", None, noon_utc());
    assert_eq!(target, "deepseek-chat");
    assert_eq!(source, RouteSource::ConditionalDefault);
}

/// An empty rule list resolves to `default_target` (validation rejects this at
/// load, but `select_at` stays total).
#[test]
fn empty_rules_use_default() {
    let cfg = config(vec![], "deepseek-chat", Some(0.0));
    let (target, source) = select_at(&cfg, "claude-sonnet", None, noon_utc());
    assert_eq!(target, "deepseek-chat");
    assert_eq!(source, RouteSource::ConditionalDefault);
}
