use super::*;
use crate::accounts::{QuotaState, StoreFamily, UsageSnapshot};
use reqwest::header::HeaderValue;

fn headers(values: &[(&'static str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in values {
        headers.append(*name, HeaderValue::from_str(value).unwrap());
    }
    headers
}

fn shared(utilization: &str, reset: u64) -> HeaderMap {
    headers(&[
        ("anthropic-ratelimit-unified-7d-utilization", utilization),
        ("anthropic-ratelimit-unified-7d-reset", &reset.to_string()),
    ])
}

fn account(name: &str) -> AccountConfig {
    AccountConfig {
        name: name.to_string(),
        store_family: Some(StoreFamily::Claude),
        ..Default::default()
    }
}

#[test]
fn strict_weekly_time_boundaries_use_both_captured_clocks() {
    let at = Instant::now();
    let unix = 1_800_000_000;
    for source in [
        Source::AnthropicHeaders,
        Source::CodexHeaders,
        Source::AnthropicUsage,
        Source::CodexUsage,
    ] {
        for (age, wall, reset, expected) in [
            (0, unix, Some(unix + 1), true),
            (300, unix + 300, Some(unix + 301), true),
            (301, unix + 301, Some(unix + 302), false),
            (0, unix, None, false),
            (0, unix, Some(unix), false),
            (1, unix + 1, Some(unix + 1), false),
            (0, unix - 1, Some(unix + 1), false),
            (0, unix, Some(unix + WINDOW_7D_SECS), true),
            (1, unix + 1, Some(unix + WINDOW_7D_SECS + 1), false),
        ] {
            let observation = Observation::new(source, at, unix, Ok((true, reset)));
            assert_eq!(
                observation.proves_exhaustion(at + Duration::from_secs(age), wall),
                expected,
                "{source:?}, age={age}, wall={wall}, reset={reset:?}"
            );
        }
        let observation = Observation::new(source, at, unix, Ok((true, Some(unix + 1))));
        assert!(!observation.proves_exhaustion(at - Duration::from_secs(1), unix));
    }
}

#[test]
fn strict_weekly_anthropic_requires_consistent_same_response_fields() {
    let at = Instant::now();
    let unix = 1_800_000_000;
    for (status, utilization, reset, expected) in [
        (None, Some("1"), Some("1800000100"), true),
        (None, Some("1.2"), Some("1800000100"), true),
        (Some("rejected"), None, Some("1800000100"), true),
        (Some("rejected"), Some("1"), Some("1800000100"), true),
        (Some("allowed"), Some("1"), Some("1800000100"), false),
        (Some("rejected"), Some("0.99"), Some("1800000100"), false),
        (Some("rejected"), Some("NaN"), Some("1800000100"), false),
        (Some("invalid"), Some("1"), Some("1800000100"), false),
        (None, Some("inf"), Some("1800000100"), false),
        (None, Some("-1"), Some("1800000100"), false),
        (None, Some("0.99"), Some("1800000100"), false),
        (Some("rejected"), None, None, false),
        (None, None, Some("1800000100"), false),
        (Some("rejected"), Some("1"), Some("invalid"), false),
    ] {
        let mut input = HeaderMap::new();
        for (name, value) in [
            ("anthropic-ratelimit-unified-7d-status", status),
            ("anthropic-ratelimit-unified-7d-utilization", utilization),
            ("anthropic-ratelimit-unified-7d-reset", reset),
        ] {
            if let Some(value) = value {
                input.insert(name, HeaderValue::from_str(value).unwrap());
            }
        }
        assert_eq!(
            Observation::anthropic(&input, at, unix)
                .unwrap()
                .proves_exhaustion(at, unix),
            expected,
            "{status:?}, {utilization:?}, {reset:?}"
        );
    }
    for duplicate in ["1", "0"] {
        let mut input = shared("1", unix + 100);
        input.append(
            "anthropic-ratelimit-unified-7d-utilization",
            HeaderValue::from_str(duplicate).unwrap(),
        );
        assert_eq!(
            Observation::anthropic(&input, at, unix)
                .unwrap()
                .proves_exhaustion(at, unix),
            duplicate == "1"
        );
    }
}

#[test]
fn strict_weekly_codex_uses_duration_and_percent_with_no_group_merge() {
    let at = Instant::now();
    let unix = 1_800_000_000;
    for (duration, percent, reset, expected) in [
        ("10080", "100", "1800000100", Some(true)),
        ("10080", "1", "1800000100", Some(false)),
        ("10080", "101", "1800000100", Some(false)),
        ("10080", "NaN", "1800000100", Some(false)),
        ("10080", "-1", "1800000100", Some(false)),
        ("10080", "", "1800000100", Some(false)),
        ("10080", "100", "", Some(false)),
        ("300", "100", "1800000100", None),
        ("invalid", "100", "1800000100", None),
    ] {
        for (minutes, utilization, reset_name) in [
            (
                "x-codex-primary-window-minutes",
                "x-codex-primary-used-percent",
                "x-codex-primary-reset-at",
            ),
            (
                "x-codex-secondary-window-minutes",
                "x-codex-secondary-used-percent",
                "x-codex-secondary-reset-at",
            ),
        ] {
            let input = headers(&[
                (minutes, duration),
                (utilization, percent),
                (reset_name, reset),
            ]);
            assert_eq!(
                Observation::codex(&input, at, unix)
                    .map(|observation| observation.proves_exhaustion(at, unix)),
                expected,
                "{minutes}, {duration}, {percent}, {reset}"
            );
        }
    }
    for (percent, reset, expected) in [
        ("100", "1800000100", true),
        ("0", "1800000100", false),
        ("100", "1800000200", false),
        ("100", "", false),
    ] {
        let input = headers(&[
            ("x-codex-primary-window-minutes", "10080"),
            ("x-codex-primary-used-percent", "100"),
            ("x-codex-primary-reset-at", "1800000100"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-secondary-used-percent", percent),
            ("x-codex-secondary-reset-at", reset),
        ]);
        assert_eq!(
            Observation::codex(&input, at, unix)
                .unwrap()
                .proves_exhaustion(at, unix),
            expected
        );
    }
}

#[test]
fn strict_weekly_requires_every_enabled_selected_identity() {
    let pool = AccountPool::new();
    let a = account("a");
    let b = account("b");
    let accounts = [a.clone(), b.clone()];
    assert!(!pool.strict_weekly_exhausted("claude", &[]));
    assert!(!pool.strict_weekly_exhausted("claude", &accounts));
    pool.note_quota("claude", &a, &shared("1", unix_now() + 3600));
    assert!(pool.strict_weekly_exhausted("claude", std::slice::from_ref(&a)));
    assert!(!pool.strict_weekly_exhausted("claude", &accounts));
    pool.note_quota("claude", &b, &shared("0.99", unix_now() + 3600));
    assert!(!pool.strict_weekly_exhausted("claude", &accounts));
    pool.note_quota("claude", &b, &shared("1", unix_now() + 3600));
    assert!(pool.strict_weekly_exhausted("claude", &accounts));
    pool.note_usage(
        "claude",
        &b,
        &UsageSnapshot {
            seven_day: Some(UsageWindow {
                utilization: 0.0,
                resets_at: Some(unix_now() + 3600),
            }),
            ..Default::default()
        },
    );
    assert!(!pool.strict_weekly_exhausted("claude", &accounts));
    let disabled = AccountConfig {
        disabled: true,
        ..b
    };
    assert!(pool.strict_weekly_exhausted("claude", &[a, disabled.clone()]));
    assert!(!pool.strict_weekly_exhausted("claude", &[disabled]));
}

#[test]
fn strict_weekly_retains_physical_store_and_inline_identity_boundaries() {
    let pool = AccountPool::new();
    let a = AccountConfig {
        uuid: Some("uuid".into()),
        ..account("a")
    };
    let alias = AccountConfig {
        name: "alias".into(),
        disabled: true,
        ..a.clone()
    };
    pool.note_quota("first", &a, &shared("1", unix_now() + 3600));
    assert!(pool.strict_weekly_exhausted("second", &[alias, a.clone()]));
    let codex = AccountConfig {
        store_family: Some(StoreFamily::Chatgpt),
        ..a
    };
    assert!(!pool.strict_weekly_exhausted("first", &[codex]));
    for store_entry in [false, true] {
        let account = AccountConfig {
            store_entry,
            ..account("inline")
        };
        pool.note_quota("first", &account, &shared("1", unix_now() + 3600));
        assert_eq!(
            pool.strict_weekly_exhausted("second", &[account]),
            store_entry
        );
    }
}

#[test]
fn strict_weekly_ignores_other_limits_without_refresh_or_quota_import() {
    let pool = AccountPool::new();
    let account = account("a");
    let key = account_key("claude", &account);
    pool.import_quotas([(
        key.clone(),
        QuotaState {
            utilization_7d: Some(1.0),
            reset_7d: Some(unix_now() + 3600),
            status_7d: Some("rejected".into()),
            ..Default::default()
        },
    )]);
    let accounts = std::slice::from_ref(&account);
    assert!(!pool.strict_weekly_exhausted("claude", accounts));
    let other_limits = headers(&[
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-utilization", "1"),
        (
            "anthropic-ratelimit-unified-5h-reset",
            &(unix_now() + 3600).to_string(),
        ),
        ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
        ("anthropic-ratelimit-unified-7d_oi-utilization", "1"),
        (
            "anthropic-ratelimit-unified-7d_oi-reset",
            &(unix_now() + 3600).to_string(),
        ),
    ]);
    pool.note_quota("claude", &account, &other_limits);
    pool.cooldown("claude", &account, Duration::from_secs(60), "quota");
    assert!(!pool.strict_weekly_exhausted("claude", accounts));
    pool.note_quota("claude", &account, &shared("1", unix_now() + 3600));
    let recorded = pool.entries.lock().unwrap()[&key]
        .strict_weekly
        .as_ref()
        .unwrap()
        .observed_at;
    pool.note_quota("claude", &account, &other_limits);
    pool.note_codex_quota(
        "claude",
        &account,
        &headers(&[("x-codex-rate-limit-reached-type", "weekly")]),
    );
    pool.note_usage("claude", &account, &UsageSnapshot::default());
    assert_eq!(
        pool.entries.lock().unwrap()[&key]
            .strict_weekly
            .as_ref()
            .unwrap()
            .observed_at,
        recorded
    );
    assert!(pool.strict_weekly_exhausted("claude", accounts));
    pool.note_quota(
        "claude",
        &account,
        &headers(&[("anthropic-ratelimit-unified-7d-utilization", "1")]),
    );
    assert!(!pool.strict_weekly_exhausted("claude", accounts));
}

#[test]
fn strict_weekly_usage_never_reuses_a_prior_reset_and_applies_clear_first() {
    let pool = AccountPool::new();
    let account = account("a");
    let accounts = std::slice::from_ref(&account);
    let reset = unix_now() + 3600;
    let usage = |utilization, resets_at| UsageSnapshot {
        seven_day: Some(UsageWindow {
            utilization,
            resets_at,
        }),
        ..Default::default()
    };
    pool.note_quota("claude", &account, &shared("1", reset));
    pool.note_usage("claude", &account, &usage(1.0, None));
    assert_eq!(
        pool.entries.lock().unwrap()[&account_key("claude", &account)]
            .quota
            .reset_7d,
        Some(reset)
    );
    assert!(!pool.strict_weekly_exhausted("claude", accounts));
    let reported = usage(1.0, Some(reset));
    pool.note_codex_usage(
        "claude",
        &account,
        &reported,
        false,
        true,
        &WeeklyUsageEvidence::from_snapshot(&reported),
    );
    assert!(pool.strict_weekly_exhausted("claude", accounts));
    pool.note_codex_usage(
        "claude",
        &account,
        &UsageSnapshot::default(),
        false,
        true,
        &WeeklyUsageEvidence::Unreported,
    );
    assert!(!pool.strict_weekly_exhausted("claude", accounts));
    for invalid in [f64::NAN, f64::INFINITY, -1.0, 1.01] {
        pool.note_codex_usage(
            "claude",
            &account,
            &usage(invalid, Some(reset)),
            false,
            false,
            &WeeklyUsageEvidence::Reported(UsageWindow {
                utilization: invalid,
                resets_at: Some(reset),
            }),
        );
        assert!(!pool.strict_weekly_exhausted("claude", accounts));
    }
}

#[test]
fn strict_weekly_codex_replaces_invalid_and_resetless_evidence() {
    let pool = AccountPool::new();
    let account = AccountConfig {
        store_family: Some(StoreFamily::Chatgpt),
        ..account("a")
    };
    let accounts = std::slice::from_ref(&account);
    let reset = (unix_now() + 3600).to_string();
    let complete = headers(&[
        ("x-codex-primary-window-minutes", "10080"),
        ("x-codex-primary-used-percent", "100"),
        ("x-codex-primary-reset-at", &reset),
    ]);
    for (name, value) in [
        ("x-codex-primary-used-percent", "invalid"),
        ("x-codex-primary-reset-at", ""),
        ("x-codex-primary-used-percent", "0"),
    ] {
        pool.note_codex_quota("arbitrary", &account, &complete);
        assert!(pool.strict_weekly_exhausted("arbitrary", accounts));
        let mut partial = complete.clone();
        partial.insert(name, HeaderValue::from_str(value).unwrap());
        pool.note_codex_quota("arbitrary", &account, &partial);
        assert!(!pool.strict_weekly_exhausted("arbitrary", accounts));
    }
    pool.note_codex_quota("arbitrary", &account, &complete);
    let mut conflict = complete.clone();
    conflict.append(
        "x-codex-primary-window-minutes",
        HeaderValue::from_static("300"),
    );
    pool.note_codex_quota("arbitrary", &account, &conflict);
    assert!(!pool.strict_weekly_exhausted("arbitrary", accounts));
    pool.note_codex_quota("arbitrary", &account, &complete);
    pool.entries
        .lock()
        .unwrap()
        .get_mut(&account_key("arbitrary", &account))
        .unwrap()
        .strict_weekly
        .as_mut()
        .unwrap()
        .observed_at -= Duration::from_secs(301);
    pool.note_codex_quota("arbitrary", &account, &HeaderMap::new());
    assert!(!pool.strict_weekly_exhausted("arbitrary", accounts));
}

#[test]
fn empty_usage_invalidation_does_not_create_or_observe_account_state() {
    let pool = AccountPool::new();
    let account = account("empty");
    pool.invalidate_weekly_usage("claude", &account, &WeeklyUsageEvidence::Invalid);
    assert!(pool.entries.lock().unwrap().is_empty());
    assert!(!pool.take_dirty());

    let key = account_key("claude", &account);
    let reset = unix_now() + 3600;
    pool.note_quota("claude", &account, &shared("1", reset));
    assert!(pool.take_dirty());
    let before = pool.export_quotas();
    pool.invalidate_weekly_usage("claude", &account, &WeeklyUsageEvidence::Unreported);
    assert!(pool.strict_weekly_exhausted("claude", std::slice::from_ref(&account)));
    pool.invalidate_weekly_usage("claude", &account, &WeeklyUsageEvidence::Invalid);
    assert!(!pool.strict_weekly_exhausted("claude", std::slice::from_ref(&account)));
    assert_eq!(pool.export_quotas(), before);
    assert!(pool.entries.lock().unwrap()[&key].observed);
    assert!(!pool.take_dirty());
}
