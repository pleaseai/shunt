use super::*;
use crate::accounts::{UsageSnapshot, UsageWindow};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod reset;

fn provider(codex: bool) -> &'static str {
    if codex {
        "codex"
    } else {
        "anthropic"
    }
}

fn credential(codex: bool, label: &str) -> (PathBuf, AccountConfig) {
    if codex {
        let token = chatgpt_access_token(label);
        let file = write_temp(
            label,
            &codex_credential_json(&token, Some("refresh"), label),
        );
        let account = codex_account_with_credentials(&file);
        (file, account)
    } else {
        let file = write_temp(label, &refreshable_claude_credential_json(Some(label)));
        let account = account_with_credentials(&file);
        (file, account)
    }
}

fn future_reset() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600
}

fn weekly_window(codex: bool, percent: Value, reset: Option<u64>) -> Value {
    if codex {
        json!({"used_percent": percent, "window_minutes": 10_080, "reset_at": reset})
    } else {
        json!({
            "utilization": percent,
            "resets_at": reset.map(|seconds| auth::shared::format_iso8601(
                UNIX_EPOCH + Duration::from_secs(seconds)
            ))
        })
    }
}

fn seed_exhaustion(pool: &AccountPool, codex: bool, account: &AccountConfig, reset: u64) {
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in [
        (
            "anthropic-ratelimit-unified-7d-utilization",
            "1".to_string(),
        ),
        (
            "anthropic-ratelimit-unified-7d-status",
            "rejected".to_string(),
        ),
        ("anthropic-ratelimit-unified-7d-reset", reset.to_string()),
    ] {
        headers.insert(name, value.parse().unwrap());
    }
    pool.note_quota(provider(codex), account, &headers);
    assert!(pool.strict_weekly_exhausted(provider(codex), std::slice::from_ref(account)));
    assert!(pool.take_dirty());
}

async fn poll_body(codex: bool, pool: &AccountPool, account: &AccountConfig, body: Value) -> bool {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(if codex {
            "/wham/usage"
        } else {
            "/api/oauth/usage"
        }))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(&server)
        .await;
    let client = reqwest::Client::new();
    let applied = if codex {
        let config = codex_backend_config(&server.uri(), account.clone());
        poll_codex_account(&client, pool, &config, "codex", &server.uri(), account).await
    } else {
        poll_account(&client, pool, "anthropic", &server.uri(), account).await
    };
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    applied
}

#[tokio::test]
async fn codex_duplicate_reports_preserve_legacy_choice_without_false_exhaustion() {
    let (file, account) = credential(true, "weekly-duplicates");
    let reset = future_reset();
    let exhausted = weekly_window(true, json!(100), Some(reset));
    for (second, normalized, strict) in [
        (exhausted.clone(), Some((1.0, Some(reset))), true),
        (
            weekly_window(true, json!(0), Some(reset)),
            Some((0.0, Some(reset))),
            false,
        ),
        (
            weekly_window(true, json!(100), Some(reset + 60)),
            Some((1.0, Some(reset + 60))),
            false,
        ),
        (
            weekly_window(true, json!(100), None),
            Some((1.0, None)),
            false,
        ),
        (weekly_window(true, json!("bad"), Some(reset)), None, false),
        (weekly_window(true, json!(150), Some(reset)), None, false),
        (
            json!({"used_percent": 100, "window_minutes": "unknown", "reset_at": reset}),
            None,
            false,
        ),
    ] {
        for reverse in [false, true] {
            for seeded in [false, true] {
                let pool = AccountPool::new();
                if seeded {
                    seed_exhaustion(&pool, true, &account, reset);
                }
                let (first, last) = if reverse {
                    (second.clone(), exhausted.clone())
                } else {
                    (exhausted.clone(), second.clone())
                };
                assert!(
                    poll_body(
                        true,
                        &pool,
                        &account,
                        json!({
                            "primary_window": first,
                            "secondary_window": last,
                        })
                    )
                    .await
                );
                assert_eq!(
                    pool.strict_weekly_exhausted("codex", std::slice::from_ref(&account)),
                    strict,
                    "reverse={reverse}, seeded={seeded}, second={second}"
                );
                let quota = only_exported_quota(&pool);
                let expected = if reverse {
                    normalized.unwrap_or((1.0, Some(reset)))
                } else {
                    (1.0, Some(reset))
                };
                assert_eq!(quota.utilization_7d, Some(expected.0));
                assert_eq!(quota.reset_7d, expected.1.or(seeded.then_some(reset)));
                assert_eq!(quota.status_7d.as_deref(), seeded.then_some("rejected"));
                assert!(pool.take_dirty());
            }
        }
    }
    std::fs::remove_file(file).unwrap();
}

#[tokio::test]
async fn malformed_weekly_reports_invalidate_strict_evidence_and_preserve_legacy_state() {
    for codex in [false, true] {
        let (file, account) = credential(codex, "weekly-invalid");
        let reset = future_reset();
        let mut malformed_windows = vec![
            Value::Null,
            weekly_window(codex, json!("bad"), Some(reset)),
            weekly_window(codex, Value::Null, Some(reset)),
        ];
        if codex {
            malformed_windows.push(weekly_window(true, json!(150), Some(reset)));
        }
        for malformed in malformed_windows {
            for partial in [false, true] {
                for seeded in [false, true] {
                    let pool = AccountPool::new();
                    if seeded {
                        seed_exhaustion(&pool, codex, &account, reset);
                    }
                    let before = pool.export_quotas();
                    let mut body = if codex {
                        json!({"secondary_window": malformed})
                    } else {
                        json!({"seven_day": malformed})
                    };
                    if partial {
                        if codex {
                            body["primary_window"] =
                                json!({"used_percent": 12, "window_minutes": 300});
                        } else {
                            body["five_hour"] = json!({"utilization": 12});
                        }
                    }
                    assert_eq!(poll_body(codex, &pool, &account, body).await, partial);
                    assert!(!pool
                        .strict_weekly_exhausted(provider(codex), std::slice::from_ref(&account)));
                    assert_eq!(pool.take_dirty(), partial);
                    let state =
                        pool.snapshot(provider(codex), std::slice::from_ref(&account), None, None);
                    assert_eq!(state[0].has_state, seeded || partial);
                    if !partial {
                        assert_eq!(pool.export_quotas(), before);
                    } else {
                        let quota = only_exported_quota(&pool);
                        assert_eq!(quota.utilization_5h, Some(0.12));
                        let retained = seeded && !(codex && malformed.is_null());
                        assert_eq!(quota.utilization_7d, retained.then_some(1.0));
                        assert_eq!(quota.reset_7d, retained.then_some(reset));
                        assert_eq!(quota.status_7d.as_deref(), retained.then_some("rejected"));
                        if retained {
                            assert_eq!(quota.observed_at_7d, before[0].1.observed_at_7d);
                        }
                    }
                }
            }
        }
        std::fs::remove_file(file).unwrap();
    }
}

#[tokio::test]
async fn claude_absent_shared_weekly_data_keeps_prior_strict_evidence() {
    let (file, account) = credential(false, "weekly-unreported");
    let reset = future_reset();
    for (body, applied) in [
        (json!({}), false),
        (json!({"five_hour": {"utilization": 12}}), true),
        (
            json!({"limits": [{
                "kind": "weekly_scoped",
                "scope": {"model": {"display_name": "Fable"}},
                "percent": 100,
                "resets_at": auth::shared::format_iso8601(UNIX_EPOCH + Duration::from_secs(reset))
            }]}),
            true,
        ),
    ] {
        let pool = AccountPool::new();
        seed_exhaustion(&pool, false, &account, reset);
        let before = only_exported_quota(&pool);
        assert_eq!(poll_body(false, &pool, &account, body).await, applied);
        assert!(pool.strict_weekly_exhausted("anthropic", std::slice::from_ref(&account)));
        let after = only_exported_quota(&pool);
        assert_eq!(after.utilization_7d, before.utilization_7d);
        assert_eq!(after.reset_7d, before.reset_7d);
        assert_eq!(after.observed_at_7d, before.observed_at_7d);
        assert_eq!(pool.take_dirty(), applied);
    }
    std::fs::remove_file(file).unwrap();
}

#[tokio::test]
async fn weekly_polls_handle_absence_resets_and_recovery() {
    for codex in [false, true] {
        let (file, account) = credential(codex, "weekly-recovery");
        let reset = future_reset();
        let pool = AccountPool::new();
        seed_exhaustion(&pool, codex, &account, reset);
        let absent = if codex {
            json!({"primary_window": null, "secondary_window": null})
        } else {
            json!({"limits": []})
        };
        let before = pool.export_quotas();
        assert!(!poll_body(codex, &pool, &account, absent).await);
        assert_eq!(
            pool.strict_weekly_exhausted(provider(codex), std::slice::from_ref(&account)),
            !codex
        );
        assert_eq!(pool.export_quotas(), before);
        assert!(!pool.take_dirty());

        let mut malformed_reset = weekly_window(codex, json!(100), Some(reset));
        malformed_reset[if codex { "reset_at" } else { "resets_at" }] = json!("bad");
        let body = if codex {
            json!({"primary_window": malformed_reset})
        } else {
            json!({"seven_day": malformed_reset})
        };
        assert!(poll_body(codex, &pool, &account, body).await);
        assert!(!pool.strict_weekly_exhausted(provider(codex), std::slice::from_ref(&account)));
        assert_eq!(only_exported_quota(&pool).reset_7d, Some(reset));

        for (percent, reported_reset, strict) in [
            (100, None, false),
            (100, Some(reset), true),
            (100, Some(reset - 7200), false),
            (100, Some(reset), true),
            (0, Some(reset), false),
        ] {
            let window = weekly_window(codex, json!(percent), reported_reset);
            let body = if codex {
                json!({"primary_window": window})
            } else {
                json!({"seven_day": window})
            };
            assert!(poll_body(codex, &pool, &account, body).await);
            assert_eq!(
                pool.strict_weekly_exhausted(provider(codex), std::slice::from_ref(&account)),
                strict
            );
            let quota = only_exported_quota(&pool);
            assert_eq!(quota.utilization_7d, Some(f64::from(percent) / 100.0));
            assert_eq!(quota.reset_7d, reported_reset.or(Some(reset)));
            assert_eq!(quota.status_7d.as_deref(), Some("rejected"));
        }
        std::fs::remove_file(file).unwrap();
    }
}

#[tokio::test]
async fn unknown_codex_duration_invalidates_even_with_valid_weekly_data() {
    let (file, account) = credential(true, "weekly-duration");
    let reset = future_reset();
    for partial in [false, true] {
        let pool = AccountPool::new();
        seed_exhaustion(&pool, true, &account, reset);
        let before = pool.export_quotas();
        let mut body = json!({"primary_window": {"used_percent": 100, "window_minutes": 60}});
        if partial {
            body["weekly_limit"] = weekly_window(true, json!(100), Some(reset));
        }
        assert_eq!(poll_body(true, &pool, &account, body).await, partial);
        assert!(!pool.strict_weekly_exhausted("codex", std::slice::from_ref(&account)));
        assert_eq!(pool.take_dirty(), partial);
        if !partial {
            assert_eq!(pool.export_quotas(), before);
        } else {
            let quota = only_exported_quota(&pool);
            assert_eq!(quota.utilization_7d, Some(1.0));
            assert_eq!(quota.reset_7d, Some(reset));
        }
    }
    std::fs::remove_file(file).unwrap();
}

#[tokio::test]
async fn empty_invalid_report_does_not_deduplicate_a_valid_alias() {
    use crate::accounts::StoreFamily;

    for codex in [false, true] {
        let (file, mut account) = credential(codex, "weekly-alias");
        account.uuid = Some("weekly-alias".to_string());
        account.store_family = Some(if codex {
            StoreFamily::Chatgpt
        } else {
            StoreFamily::Claude
        });
        let alias = AccountConfig {
            name: "later-alias".to_string(),
            ..account.clone()
        };
        let server = MockServer::start().await;
        let invalid = weekly_window(codex, json!("bad"), Some(future_reset()));
        let empty = if codex {
            json!({"primary_window": invalid})
        } else {
            json!({"seven_day": invalid})
        };
        let valid = if codex {
            json!({"primary_window": {"used_percent": 12, "window_minutes": 300}})
        } else {
            json!({"five_hour": {"utilization": 12}})
        };
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty))
            .up_to_n_times(1)
            .with_priority(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(valid))
            .with_priority(2)
            .expect(1)
            .mount(&server)
            .await;
        let mut config = Config::default();
        config.providers.retain(|name, _| name == provider(codex));
        config.server.default_provider = provider(codex).to_string();
        config.upstream_order = vec![provider(codex).to_string()];
        let upstream = config.providers.get_mut(provider(codex)).unwrap();
        upstream.auth = if codex {
            AuthMode::ChatgptOauth
        } else {
            AuthMode::ClaudeOauth
        };
        upstream.base_url = server.uri();
        upstream.accounts = vec![account.clone(), alias];
        let state = AppState::new(config, reqwest::Client::new()).unwrap();
        seed_exhaustion(&state.accounts, codex, &account, future_reset());
        poll_all(&state).await;
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        assert!(!state
            .accounts
            .strict_weekly_exhausted(provider(codex), std::slice::from_ref(&account)));
        let quota = only_exported_quota(&state.accounts);
        assert_eq!(quota.utilization_5h, Some(0.12));
        assert_eq!(quota.utilization_7d, (!codex).then_some(1.0));
        std::fs::remove_file(file).unwrap();
    }
}

#[test]
fn normalized_usage_callers_keep_their_original_observation_behavior() {
    let pool = AccountPool::new();
    let account = AccountConfig {
        name: "normalized".to_string(),
        ..Default::default()
    };
    let reset = future_reset();
    seed_exhaustion(&pool, false, &account, reset);
    pool.note_usage("anthropic", &account, &UsageSnapshot::default());
    assert!(pool.strict_weekly_exhausted("anthropic", std::slice::from_ref(&account)));
    assert!(pool.take_dirty());
    pool.note_usage(
        "anthropic",
        &account,
        &UsageSnapshot {
            seven_day: Some(UsageWindow {
                utilization: 0.0,
                resets_at: Some(reset),
            }),
            ..Default::default()
        },
    );
    assert!(!pool.strict_weekly_exhausted("anthropic", std::slice::from_ref(&account)));
}
