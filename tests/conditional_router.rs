//! Public-API tests for `[models.router] type = "conditional"`.
//!
//! The unit tests under `src/routing/conditional/tests.rs` cover the rule walk
//! and the pure predicates. These pin the config surface: serde round-trip,
//! validation, and the types an operator writes in `shunt.toml`.

use shunt::config::{
    parse_hh_mm, ConditionalRouterConfig, ConditionalRule, HeaderMatch, HeaderMatchMode,
    RouterConfig, TimeBetween, Weekday, WhenCondition,
};

/// The conditional router type deserializes from TOML-shaped JSON.
#[test]
fn serde_roundtrip() {
    let router = RouterConfig::Conditional(ConditionalRouterConfig {
        utc_offset_hours: Some(8.0),
        rules: vec![ConditionalRule {
            name: "offpeak".into(),
            priority: 1,
            target: "deepseek-chat".into(),
            when: WhenCondition {
                time_between: Some(TimeBetween {
                    start: (22, 0),
                    end: (8, 0),
                }),
                header: None,
                model_starts_with: None,
                days: None,
            },
        }],
        default_target: "glm-4-flash".into(),
    });

    let json = serde_json::to_value(&router).unwrap();
    assert_eq!(json["type"], "conditional");
    assert_eq!(json["utc_offset_hours"], 8.0);
    assert_eq!(json["rules"][0]["name"], "offpeak");
    assert_eq!(json["rules"][0]["when"]["time_between"]["start"], "22:00");

    let round: RouterConfig = serde_json::from_value(json).unwrap();
    assert_eq!(router, round);
}

/// HH:MM parsing is available for config validation tests.
#[test]
fn parse_hh_mm_valid() {
    assert_eq!(parse_hh_mm("22:00"), Some((22, 0)));
    assert_eq!(parse_hh_mm("00:00"), Some((0, 0)));
    assert_eq!(parse_hh_mm("23:59"), Some((23, 59)));
}

#[test]
fn parse_hh_mm_invalid() {
    assert_eq!(parse_hh_mm("24:00"), None);
    assert_eq!(parse_hh_mm("12:60"), None);
    assert_eq!(parse_hh_mm("abc"), None);
    assert_eq!(parse_hh_mm(""), None);
}

/// Days serialize as lowercase snake_case.
#[test]
fn weekday_serde() {
    let day = serde_json::to_value(Weekday::Sat).unwrap();
    assert_eq!(day, "sat");
    let day: Weekday = serde_json::from_value(serde_json::json!("sun")).unwrap();
    assert_eq!(day, Weekday::Sun);
}

/// Header match modes serialize as snake_case.
#[test]
fn header_mode_serde() {
    let mode = serde_json::to_value(HeaderMatchMode::Contains).unwrap();
    assert_eq!(mode, "contains");
    let mode: HeaderMatchMode = serde_json::from_value(serde_json::json!("starts_with")).unwrap();
    assert_eq!(mode, HeaderMatchMode::StartsWith);
}

/// TimeBetween round-trips through the wire format.
#[test]
fn time_between_serde() {
    let tb = TimeBetween {
        start: (22, 0),
        end: (8, 0),
    };
    let json = serde_json::to_value(&tb).unwrap();
    assert_eq!(json["start"], "22:00");
    assert_eq!(json["end"], "08:00");
    let round: TimeBetween = serde_json::from_value(json).unwrap();
    assert_eq!(tb, round);
}

// ── Config validation ────────────────────────────────────────────────────────

use std::collections::BTreeMap;

use shunt::config::{
    AuthMode, CountTokens, ModelConfig, ProviderKind, RetryConfig, UpstreamAuth, UpstreamConfig,
};

/// A minimal one-upstream config whose only model carries the given router and
/// whose `default_target`/target ids resolve to a real upstream-backed alias.
fn config_with_router(router: RouterConfig, default_target: &str) -> shunt::config::Config {
    let mut config = shunt::config::Config::default();
    config.providers.clear();
    config.upstreams = vec![UpstreamConfig {
        name: "local".into(),
        provider: None,
        kind: Some(ProviderKind::Anthropic),
        base_url: Some("http://127.0.0.1:9".into()),
        auth: Some(UpstreamAuth::Shorthand(AuthMode::Passthrough)),
        effort: None,
        service_tier: None,
        classifier_model: None,
        count_tokens: CountTokens::Tiktoken,
        websocket: false,
        tool_search: None,
        request_compression: true,
        retry: RetryConfig {
            max_retries: 0,
            ..RetryConfig::default()
        },
        workspace_roots: Vec::new(),
        profile_dir: None,
        sandbox: true,
    }];
    config.server.default_provider = "local".into();
    config.models = vec![
        ModelConfig {
            id: "cost-optimized".into(),
            display_name: None,
            upstream_model: None,
            router: Some(router),
            stage_router: None,
            subagents: None,
        },
        ModelConfig {
            id: default_target.into(),
            display_name: None,
            upstream_model: Some(BTreeMap::from([("local".into(), "m".into())])),
            router: None,
            stage_router: None,
            subagents: None,
        },
    ];
    config
}

fn conditional(
    rules: Vec<ConditionalRule>,
    default_target: &str,
    utc: Option<f64>,
) -> RouterConfig {
    RouterConfig::Conditional(ConditionalRouterConfig {
        utc_offset_hours: utc,
        rules,
        default_target: default_target.into(),
    })
}

/// A catch-all rule is rejected: it would shadow every later rule.
#[test]
fn catch_all_rule_is_rejected() {
    let cfg = config_with_router(
        conditional(
            vec![ConditionalRule {
                name: "noop".into(),
                priority: 1,
                target: "fallback".into(),
                when: WhenCondition::default(),
            }],
            "fallback",
            Some(8.0),
        ),
        "fallback",
    );
    let err = cfg.validate().expect_err("catch-all must be rejected");
    assert!(err.to_string().contains("catch-all"), "got: {err}");
}

/// An out-of-range UTC offset is rejected.
#[test]
fn bad_utc_offset_is_rejected() {
    let cfg = config_with_router(
        conditional(
            vec![ConditionalRule {
                name: "r".into(),
                priority: 1,
                target: "fallback".into(),
                when: WhenCondition {
                    model_starts_with: Some("claude".into()),
                    ..Default::default()
                },
            }],
            "fallback",
            Some(99.0),
        ),
        "fallback",
    );
    let err = cfg.validate().expect_err("99h offset must be rejected");
    assert!(err.to_string().contains("utc_offset_hours"), "got: {err}");
}

/// A NaN UTC offset is rejected (NaN fails every range test).
#[test]
fn nan_utc_offset_is_rejected() {
    let cfg = config_with_router(
        conditional(
            vec![ConditionalRule {
                name: "r".into(),
                priority: 1,
                target: "fallback".into(),
                when: WhenCondition {
                    model_starts_with: Some("claude".into()),
                    ..Default::default()
                },
            }],
            "fallback",
            Some(f64::NAN),
        ),
        "fallback",
    );
    let err = cfg.validate().expect_err("NaN offset must be rejected");
    assert!(err.to_string().contains("utc_offset_hours"), "got: {err}");
}

/// An empty rules list is rejected.
#[test]
fn empty_rules_are_rejected() {
    let cfg = config_with_router(conditional(vec![], "fallback", Some(8.0)), "fallback");
    let err = cfg.validate().expect_err("empty rules must be rejected");
    assert!(err.to_string().contains("at least one rule"), "got: {err}");
}

/// Valid config passes validation and the rules are returned in priority order.
#[test]
fn valid_config_passes_and_is_sorted() {
    let cfg = config_with_router(
        conditional(
            vec![
                ConditionalRule {
                    name: "later".into(),
                    priority: 10,
                    target: "fallback".into(),
                    when: WhenCondition {
                        model_starts_with: Some("claude".into()),
                        ..Default::default()
                    },
                },
                ConditionalRule {
                    name: "earlier".into(),
                    priority: 1,
                    target: "fallback".into(),
                    when: WhenCondition {
                        model_starts_with: Some("claude".into()),
                        ..Default::default()
                    },
                },
            ],
            "fallback",
            Some(8.0),
        ),
        "fallback",
    );
    let validated = cfg.validate().expect("valid config");
    let model = validated
        .models
        .iter()
        .find(|m| m.id == "cost-optimized")
        .expect("model present");
    let RouterConfig::Conditional(cond) = model.router.as_ref().unwrap() else {
        panic!("expected conditional router");
    };
    assert_eq!(
        cond.rules[0].name, "earlier",
        "priority order applied at load"
    );
    assert_eq!(cond.rules[1].name, "later");
}

/// A header name padded with whitespace is rejected: it is matched against
/// inbound header names exactly, so a padded name would silently never fire.
#[test]
fn padded_header_name_is_rejected() {
    let cfg = config_with_router(
        conditional(
            vec![ConditionalRule {
                name: "r".into(),
                priority: 1,
                target: "fallback".into(),
                when: WhenCondition {
                    header: Some(HeaderMatch {
                        name: " x-tier ".into(),
                        value: "vip".into(),
                        mode: HeaderMatchMode::Equals,
                    }),
                    ..Default::default()
                },
            }],
            "fallback",
            Some(8.0),
        ),
        "fallback",
    );
    let err = cfg
        .validate()
        .expect_err("padded header name must be rejected");
    assert!(err.to_string().contains("padded"), "got: {err}");
}

/// A blank header condition name is rejected.
#[test]
fn blank_header_name_is_rejected() {
    let cfg = config_with_router(
        conditional(
            vec![ConditionalRule {
                name: "r".into(),
                priority: 1,
                target: "fallback".into(),
                when: WhenCondition {
                    header: Some(HeaderMatch {
                        name: "  ".into(),
                        value: "x".into(),
                        mode: HeaderMatchMode::Equals,
                    }),
                    ..Default::default()
                },
            }],
            "fallback",
            Some(8.0),
        ),
        "fallback",
    );
    let err = cfg
        .validate()
        .expect_err("blank header name must be rejected");
    assert!(err.to_string().contains("blank"), "got: {err}");
}

// ── End-to-end resolution ────────────────────────────────────────────────────

/// Resolution through a real conditional entry lands on the rule's target and
/// re-stamps the client-requested id, exercising the `resolve_chain` arm, the
/// `algorithm()` label, and the outcome wiring.
#[test]
fn resolve_model_routes_through_a_conditional_entry() {
    let cfg = config_with_router(
        conditional(
            vec![ConditionalRule {
                name: "sonnet-to-cheap".into(),
                priority: 1,
                target: "fallback".into(),
                when: WhenCondition {
                    model_starts_with: Some("claude-sonnet".into()),
                    ..Default::default()
                },
            }],
            "fallback",
            Some(0.0),
        ),
        "fallback",
    );
    let cfg = cfg.validate().expect("valid config");

    // A body-less resolution has no headers, so the model-prefix rule (no
    // header condition) still applies.
    let route = shunt::routing::resolve_model(&cfg, "claude-sonnet-4-5-20260514");
    assert_eq!(
        route.provider, "local",
        "the target's upstream backs the route"
    );
    assert_eq!(
        route.model, "claude-sonnet-4-5-20260514",
        "the client is told the id it asked for"
    );
}

/// A body-less resolution with no matching rule reports `default_target`.
#[test]
fn resolve_model_falls_through_to_default_target() {
    let cfg = config_with_router(
        conditional(
            vec![ConditionalRule {
                name: "only-opus".into(),
                priority: 1,
                target: "fallback".into(),
                when: WhenCondition {
                    model_starts_with: Some("claude-opus".into()),
                    ..Default::default()
                },
            }],
            "fallback",
            Some(0.0),
        ),
        "fallback",
    );
    let cfg = cfg.validate().expect("valid config");

    let route = shunt::routing::resolve_model(&cfg, "claude-sonnet-4-5-20260514");
    assert_eq!(route.provider, "local");
}

/// A blank `model_starts_with` prefix is rejected.
#[test]
fn blank_model_prefix_is_rejected() {
    let cfg = config_with_router(
        conditional(
            vec![ConditionalRule {
                name: "r".into(),
                priority: 1,
                target: "fallback".into(),
                when: WhenCondition {
                    model_starts_with: Some(String::new()),
                    ..Default::default()
                },
            }],
            "fallback",
            Some(8.0),
        ),
        "fallback",
    );
    let err = cfg.validate().expect_err("blank prefix must be rejected");
    assert!(
        err.to_string().contains("blank model_starts_with"),
        "got: {err}"
    );
}

/// An out-of-range `HH:MM` built programmatically is rejected (the deserializer
/// only guards the parsed path).
#[test]
fn out_of_range_window_is_rejected() {
    let cfg = config_with_router(
        conditional(
            vec![ConditionalRule {
                name: "r".into(),
                priority: 1,
                target: "fallback".into(),
                when: WhenCondition {
                    time_between: Some(TimeBetween {
                        start: (25, 0),
                        end: (8, 0),
                    }),
                    ..Default::default()
                },
            }],
            "fallback",
            Some(8.0),
        ),
        "fallback",
    );
    let err = cfg.validate().expect_err("25:00 must be rejected");
    assert!(err.to_string().contains("time_between"), "got: {err}");
}

/// An empty `days` list is rejected.
#[test]
fn empty_days_list_is_rejected() {
    let cfg = config_with_router(
        conditional(
            vec![ConditionalRule {
                name: "r".into(),
                priority: 1,
                target: "fallback".into(),
                when: WhenCondition {
                    days: Some(Vec::new()),
                    ..Default::default()
                },
            }],
            "fallback",
            Some(8.0),
        ),
        "fallback",
    );
    let err = cfg.validate().expect_err("empty days must be rejected");
    assert!(err.to_string().contains("days"), "got: {err}");
}
