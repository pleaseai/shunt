use super::*;

#[tokio::test]
async fn claude_poller_validates_fractional_resets_without_legacy_changes() {
    let (file, account) = credential(false, "weekly-reset-validation");
    let credential_before = std::fs::read(&file).unwrap();
    let metadata_before = std::fs::metadata(&file).unwrap();
    let reset = future_reset();
    let canonical = auth::shared::format_iso8601(UNIX_EPOCH + Duration::from_secs(reset));
    let seconds = canonical.strip_suffix('Z').unwrap();

    for (suffix, strict) in [
        ("Z", true),
        ("z", true),
        (".123456Z", true),
        (".123456+00:00", true),
        ("+00:00", true),
        ("-00:00", true),
        (".garbageZ", false),
        (".Z", false),
        (".1.2Z", false),
        (".1e2Z", false),
    ] {
        for seeded in [false, true] {
            let pool = AccountPool::new();
            if seeded {
                seed_exhaustion(&pool, false, &account, reset + 600);
            }
            let raw_reset = format!("{seconds}{suffix}");
            assert!(
                poll_body(
                    false,
                    &pool,
                    &account,
                    json!({
                        "seven_day": {"utilization": 100, "resets_at": raw_reset}
                    })
                )
                .await
            );
            assert_eq!(
                pool.strict_weekly_exhausted("anthropic", std::slice::from_ref(&account)),
                strict,
                "seeded={seeded}, reset={raw_reset}"
            );
            assert!(pool.take_dirty());
            let legacy = only_exported_quota(&pool);
            assert_eq!(legacy.utilization_7d, Some(1.0));
            assert_eq!(legacy.reset_7d, Some(reset));
            assert_eq!(legacy.status_7d.as_deref(), seeded.then_some("rejected"));
            assert!(legacy.observed_at_7d.is_some());
            let snapshot = pool.snapshot("anthropic", std::slice::from_ref(&account), None, None);
            assert!(snapshot[0].has_state);

            if !strict {
                assert!(
                    poll_body(
                        false,
                        &pool,
                        &account,
                        json!({
                            "seven_day": {"utilization": 100, "resets_at": canonical}
                        })
                    )
                    .await
                );
                assert!(pool.strict_weekly_exhausted("anthropic", std::slice::from_ref(&account)));
                assert_eq!(only_exported_quota(&pool).reset_7d, Some(reset));
            }
        }
    }
    let metadata_after = std::fs::metadata(&file).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), credential_before);
    assert_eq!(metadata_after.len(), metadata_before.len());
    assert_eq!(
        metadata_after.modified().unwrap(),
        metadata_before.modified().unwrap()
    );
    std::fs::remove_file(file).unwrap();
}
