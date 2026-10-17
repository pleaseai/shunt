//! `import_all` against SQLite and PostgreSQL through the contract harness.
//! Failures are injected inside the engine (a trigger that aborts the
//! `import_record` insert, after the rows were already written).

use std::path::{Path, PathBuf};

use super::{import_all, ImportSources, Outcome, Source, Subsystem};
use crate::accounts::{AccountKey, AccountPool, AccountStateIdentity, QuotaState, StoreFamily};
use crate::gateway::spend::persist::parse_state;
use crate::gateway::spend::store::{Period, Scope, SpendState, SpendStore};
use crate::store::contract::store_contract_test;
use crate::store::marker::{self, ImportMarker};
use crate::store::sessions_repo::load_sessions;
use crate::store::spend_repo::load_spend;
use crate::store::test_support::TempDir;
use crate::store::{pool_repo, Db, Store, StoreError};

/// One clock reading per test process, so a fixture built twice is equal.
fn unix_now() -> u64 {
    static NOW: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *NOW.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    })
}

/// An in-window reset: far-future resets are not plausible for a 5 h window
/// and normalization drops their utilization.
fn reset() -> u64 {
    unix_now() + 3_600
}

fn observed() -> u64 {
    unix_now() - 60
}

fn key(name: &str) -> AccountKey {
    AccountKey {
        store_family: StoreFamily::Claude,
        identity: AccountStateIdentity::StoreEntry {
            name: name.to_string(),
        },
    }
}

fn quota(utilization: f64) -> QuotaState {
    QuotaState {
        utilization_5h: Some(utilization),
        reset_5h: Some(reset()),
        status: Some("allowed".to_string()),
        observed_at_5h: Some(observed()),
        // Stamped here so normalization never fills it from the wall clock:
        // the import and the expected value are normalized at different
        // instants and would disagree across a second boundary.
        observed_at_status: Some(observed()),
        ..Default::default()
    }
}

fn pool_snapshot(version: u32, quotas: &[(AccountKey, QuotaState)]) -> Vec<u8> {
    let accounts: Vec<_> = quotas
        .iter()
        .map(|(k, q)| serde_json::json!({"key": k, "quota": q}))
        .collect();
    serde_json::to_vec_pretty(&serde_json::json!({"version": version, "accounts": accounts}))
        .unwrap()
}

fn v3_pool() -> Vec<u8> {
    pool_snapshot(3, &[(key("a"), quota(0.25)), (key("b"), quota(0.5))])
}

/// A v2 account: status and utilization stamps share the legacy combined
/// timestamp, so the legacy path has to give the status its own stamp.
fn v2_quota() -> QuotaState {
    QuotaState {
        utilization_5h: Some(0.4),
        reset_5h: Some(reset()),
        status_5h: Some("allowed".to_string()),
        observed_at_5h: Some(observed()),
        ..Default::default()
    }
}

fn identity_json(sub: &str) -> serde_json::Value {
    serde_json::json!({"sub": sub, "email": sub, "name": "dev"})
}

fn gateway_snapshot() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "refresh_tokens": [
            {"token_sha256": "a".repeat(64), "identity": identity_json("a@x.io"),
             "family": "fam-a", "issued_at": 1_000_000},
            {"token_sha256": "b".repeat(64), "identity": identity_json("a@x.io"),
             "family": "fam-a", "inactive_since": 1_000_500, "issued_at": 1_000_000},
        ]
    }))
    .unwrap()
}

fn spend_state() -> SpendState {
    let mut state = SpendState {
        next_audit_id: 1,
        ..SpendState::default()
    };
    for (n, (period, amount)) in [(Period::Monthly, "100"), (Period::Daily, "5")]
        .into_iter()
        .enumerate()
    {
        state = SpendStore::upsert_state(
            state,
            Scope::Organization,
            period,
            Some(amount.to_string()),
            "admin-key:writer",
            format!("2026-10-01T00:00:{n:02}.000Z"),
        )
        .0;
    }
    state
}

/// Typed records plus one opaque limit (between the typed ones) and one opaque
/// audit record, so the snapshot exercises the verbatim-preservation path.
fn spend_snapshot() -> Vec<u8> {
    let state = spend_state();
    let mut limits: Vec<_> = state
        .limits
        .iter()
        .map(|l| serde_json::to_value(l).unwrap())
        .collect();
    limits.insert(1, serde_json::json!({"id": "spl_odd", "amount": 7}));
    let mut audit: Vec<_> = state
        .audit
        .iter()
        .map(|r| serde_json::to_value(r).unwrap())
        .collect();
    audit.push(serde_json::json!({"id": 77, "future_field": [1, 2]}));
    serde_json::to_vec(&serde_json::json!({
        "version": 1, "limits": limits, "audit": audit, "next_audit_id": state.next_audit_id,
    }))
    .unwrap()
}

fn only(subsystem: Subsystem, snapshot: Option<&Path>) -> ImportSources {
    let on = Source::On {
        snapshot: snapshot.map(Path::to_path_buf),
    };
    let mut sources = ImportSources::default();
    match subsystem {
        Subsystem::Pool => sources.pool = on,
        Subsystem::Gateway => sources.gateway = on,
        Subsystem::Spend => sources.spend = on,
    }
    sources
}

fn all_on(pool: &Path, gateway: &Path, spend: &Path) -> ImportSources {
    ImportSources {
        pool: Source::On {
            snapshot: Some(pool.to_path_buf()),
        },
        gateway: Source::On {
            snapshot: Some(gateway.to_path_buf()),
        },
        spend: Source::On {
            snapshot: Some(spend.to_path_buf()),
        },
    }
}

fn put(dir: &TempDir, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.file(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

async fn count(store: &Store, table: &str) -> i64 {
    let mut tx = store.db().begin().await.unwrap();
    let row = tx
        .fetch_optional(&format!("SELECT COUNT(*) FROM {table}"), &[])
        .await
        .unwrap()
        .unwrap();
    tx.rollback().await.unwrap();
    row.int(0).unwrap()
}

/// `(imported_at, source_path, source_sha256, row_count)` of one record.
async fn record(
    store: &Store,
    subsystem: &str,
) -> Option<(i64, Option<String>, Option<String>, i64)> {
    let mut tx = store.db().begin().await.unwrap();
    let row = tx
        .fetch_optional(
            "SELECT imported_at, source_path, source_sha256, row_count \
             FROM import_record WHERE subsystem = $1",
            &[subsystem.into()],
        )
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    row.map(|r| {
        (
            r.int(0).unwrap(),
            r.opt_text(1).unwrap(),
            r.opt_text(2).unwrap(),
            r.int(3).unwrap(),
        )
    })
}

async fn run(store: &Store, statements: &[String]) {
    let mut tx = store.db().begin().await.unwrap();
    for statement in statements {
        tx.execute(statement, &[]).await.unwrap();
    }
    tx.commit().await.unwrap();
}

/// Make inserting an `import_record` abort in the engine.
async fn fail_record_insert(store: &Store) {
    let statements = match store.db() {
        Db::Sqlite(_) => vec![
            "CREATE TRIGGER fail_import_record BEFORE INSERT ON import_record \
             BEGIN SELECT RAISE(ABORT, 'injected'); END"
                .to_string(),
        ],
        #[cfg(feature = "store-postgres")]
        Db::Postgres(_) => vec![
            "CREATE OR REPLACE FUNCTION fail_import_fn() RETURNS trigger AS \
             $body$ BEGIN RAISE EXCEPTION 'injected'; END $body$ LANGUAGE plpgsql"
                .to_string(),
            "CREATE TRIGGER fail_import_record BEFORE INSERT ON import_record \
             FOR EACH ROW EXECUTE FUNCTION fail_import_fn()"
                .to_string(),
        ],
    };
    run(store, &statements).await;
}

async fn allow_record_insert(store: &Store) {
    let statement = match store.db() {
        Db::Sqlite(_) => "DROP TRIGGER fail_import_record",
        #[cfg(feature = "store-postgres")]
        Db::Postgres(_) => "DROP TRIGGER fail_import_record ON import_record",
    };
    run(store, &[statement.to_string()]).await;
}

async fn data_rows(store: &Store) -> i64 {
    let mut total = 0;
    for table in [
        "pool_quota",
        "refresh_token",
        "spend_limit",
        "spend_audit",
        "spend_meta",
    ] {
        total += count(store, table).await;
    }
    total
}

store_contract_test!(v3_pool_snapshot_imports_rows_and_record, |store| {
    let dir = TempDir::new("import-pool");
    let bytes = v3_pool();
    let path = put(&dir, "pool.json", &bytes);

    let report = import_all(&store, &only(Subsystem::Pool, Some(&path)))
        .await
        .unwrap();
    assert_eq!(report.pool, Outcome::Imported { rows: 2 });
    assert_eq!(report.gateway, Outcome::Off);
    assert_eq!(report.spend, Outcome::Off);

    let loaded = pool_repo::load_pool(&store).await.unwrap();
    let expected = crate::state_persist::normalized_quotas(&bytes).unwrap();
    assert_eq!(
        loaded
            .iter()
            .cloned()
            .collect::<std::collections::HashMap<_, _>>(),
        expected
            .iter()
            .cloned()
            .collect::<std::collections::HashMap<_, _>>()
    );
    let by_key: std::collections::HashMap<_, _> = loaded.into_iter().collect();
    assert_eq!(by_key[&key("a")].utilization_5h, Some(0.25));
    assert!(by_key[&key("b")].reset_5h.is_some());

    let (imported_at, source_path, sha, rows) = record(&store, "pool").await.unwrap();
    assert!(imported_at > 1_700_000_000);
    assert_eq!(source_path.as_deref(), Some(path.to_str().unwrap()));
    assert_eq!(sha.as_deref(), Some(marker::sha256_hex(&bytes).as_str()));
    assert_eq!(rows, 2);
    assert_eq!(
        count(&store, "import_record").await,
        1,
        "only pool recorded"
    );
});

store_contract_test!(v2_pool_snapshot_imports_with_v3_normalization, |store| {
    let dir = TempDir::new("import-pool-v2");
    let bytes = pool_snapshot(2, &[(key("legacy"), v2_quota())]);
    let path = put(&dir, "pool.json", &bytes);

    let report = import_all(&store, &only(Subsystem::Pool, Some(&path)))
        .await
        .unwrap();
    assert_eq!(report.pool, Outcome::Imported { rows: 1 });

    let scratch = AccountPool::new();
    scratch.import_quotas_legacy([(key("legacy"), v2_quota())]);
    let want = scratch.export_quotas();
    let plain = AccountPool::new();
    plain.import_quotas([(key("legacy"), v2_quota())]);
    assert_ne!(
        want,
        plain.export_quotas(),
        "fixture must tell the paths apart"
    );

    let loaded = pool_repo::load_pool(&store).await.unwrap();
    assert_eq!(loaded, want);
    assert_eq!(
        loaded[0].1.observed_at_status_5h, loaded[0].1.observed_at_5h,
        "the legacy path gives the surviving status its own stamp"
    );
});

store_contract_test!(gateway_snapshot_imports_sessions_and_tombstones, |store| {
    let dir = TempDir::new("import-gateway");
    let bytes = gateway_snapshot();
    let path = put(&dir, "sessions.json", &bytes);

    let report = import_all(&store, &only(Subsystem::Gateway, Some(&path)))
        .await
        .unwrap();
    assert_eq!(report.gateway, Outcome::Imported { rows: 2 });

    let loaded = load_sessions(&store).await.unwrap();
    assert_eq!(loaded.len(), 2);
    let active = loaded
        .iter()
        .find(|r| r.token_sha256 == "a".repeat(64))
        .unwrap();
    assert_eq!(
        (active.inactive_since, active.family.as_str()),
        (None, "fam-a")
    );
    let tombstone = loaded
        .iter()
        .find(|r| r.token_sha256 == "b".repeat(64))
        .unwrap();
    assert_eq!(tombstone.inactive_since, Some(1_000_500));
    assert_eq!(tombstone.identity.sub, "a@x.io");
    assert_eq!(record(&store, "gateway").await.unwrap().3, 2);
});

store_contract_test!(
    spend_snapshot_imports_with_opaque_records_verbatim,
    |store| {
        let dir = TempDir::new("import-spend");
        let bytes = spend_snapshot();
        let path = put(&dir, "spend.json", &bytes);

        let report = import_all(&store, &only(Subsystem::Spend, Some(&path)))
            .await
            .unwrap();
        // 2 typed + 1 opaque limit, 2 typed + 1 opaque audit record.
        assert_eq!(report.spend, Outcome::Imported { rows: 6 });

        let loaded = load_spend(&store).await.unwrap().unwrap();
        assert_eq!(loaded, parse_state(&bytes, &path).unwrap());
        assert_eq!(loaded.limits.len(), 2);
        assert_eq!(loaded.opaque_limits.len(), 1);
        assert_eq!(loaded.opaque_limits[0].typed_before, 1);
        assert_eq!(
            loaded.opaque_limits[0].value,
            serde_json::json!({"id": "spl_odd", "amount": 7})
        );
        assert_eq!(
            loaded.opaque_audit[0].value,
            serde_json::json!({"id": 77, "future_field": [1, 2]})
        );
        assert!(
            loaded.next_audit_id >= 78,
            "opaque ids still advance the counter"
        );
        assert_eq!(record(&store, "spend").await.unwrap().3, 6);
    }
);

store_contract_test!(an_empty_spend_snapshot_still_writes_its_meta_row, |store| {
    let dir = TempDir::new("import-spend-empty");
    let bytes = br#"{"version":1,"limits":[],"audit":[],"next_audit_id":1}"#;
    let path = put(&dir, "spend.json", bytes);
    let report = import_all(&store, &only(Subsystem::Spend, Some(&path)))
        .await
        .unwrap();
    assert_eq!(report.spend, Outcome::Imported { rows: 0 });
    let loaded = load_spend(&store).await.unwrap();
    assert_eq!(
        loaded,
        Some(SpendState {
            next_audit_id: 1,
            ..SpendState::default()
        })
    );
});

store_contract_test!(no_snapshot_writes_a_null_source_record, |store| {
    let dir = TempDir::new("import-none");
    let absent = dir.file("never-written.json");
    let sources = ImportSources {
        pool: Source::On { snapshot: None },
        gateway: Source::On {
            snapshot: Some(absent.clone()),
        },
        spend: Source::Off,
    };
    let report = import_all(&store, &sources).await.unwrap();
    assert_eq!(report.pool, Outcome::NoSnapshot);
    assert_eq!(report.gateway, Outcome::NoSnapshot);
    assert_eq!(report.spend, Outcome::Off);

    for name in ["pool", "gateway"] {
        let (_, source_path, sha, rows) = record(&store, name).await.unwrap();
        assert_eq!((source_path, sha, rows), (None, None, 0), "{name}");
    }
    assert!(
        record(&store, "spend").await.is_none(),
        "an Off subsystem is not recorded"
    );
    assert!(
        !marker::sidecar_path(&absent).exists(),
        "nothing imported, no sidecar"
    );
});

store_contract_test!(
    a_present_record_never_reimports_even_when_tables_are_empty,
    |store| {
        let dir = TempDir::new("import-once");
        let pool = put(&dir, "pool.json", &v3_pool());
        let gateway = put(&dir, "sessions.json", &gateway_snapshot());
        let spend = put(&dir, "spend.json", &spend_snapshot());
        let sources = all_on(&pool, &gateway, &spend);

        import_all(&store, &sources).await.unwrap();
        assert!(data_rows(&store).await > 0);

        // The operator clears everything through the product (here: SQL); the old
        // JSON files are still on disk.
        let clear: Vec<String> = [
            "pool_quota",
            "refresh_token",
            "spend_limit",
            "spend_audit",
            "spend_meta",
        ]
        .iter()
        .map(|t| format!("DELETE FROM {t}"))
        .collect();
        run(&store, &clear).await;

        let report = import_all(&store, &sources).await.unwrap();
        assert_eq!(report.pool, Outcome::AlreadyImported);
        assert_eq!(report.gateway, Outcome::AlreadyImported);
        assert_eq!(report.spend, Outcome::AlreadyImported);
        assert_eq!(data_rows(&store).await, 0, "no re-import");
    }
);

store_contract_test!(
    a_snapshot_that_appears_after_a_no_snapshot_boot_is_ignored,
    |store| {
        let dir = TempDir::new("import-late");
        let path = dir.file("pool.json");
        let sources = only(Subsystem::Pool, Some(&path));

        assert_eq!(
            import_all(&store, &sources).await.unwrap().pool,
            Outcome::NoSnapshot
        );
        std::fs::write(&path, v3_pool()).unwrap();

        assert_eq!(
            import_all(&store, &sources).await.unwrap().pool,
            Outcome::AlreadyImported
        );
        assert!(pool_repo::load_pool(&store).await.unwrap().is_empty());
    }
);

store_contract_test!(
    bad_snapshots_leave_nothing_and_name_subsystem_and_path,
    |store| {
        let dir = TempDir::new("import-bad");
        let truncated_gateway = {
            let bytes = gateway_snapshot();
            bytes[..bytes.len() / 2].to_vec()
        };
        let cases: Vec<(Subsystem, &str, Vec<u8>, &str)> = vec![
            (
                Subsystem::Pool,
                "pool-json",
                b"{ not json".to_vec(),
                "not valid json",
            ),
            (
                Subsystem::Pool,
                "pool-version",
                br#"{"version":9,"accounts":[]}"#.to_vec(),
                "version mismatch",
            ),
            (
                Subsystem::Gateway,
                "gw-truncated",
                truncated_gateway,
                "not valid json",
            ),
            (
                Subsystem::Gateway,
                "gw-duplicate",
                serde_json::to_vec(&serde_json::json!({
                    "version": 1,
                    "refresh_tokens": [
                        {"token_sha256": "a".repeat(64), "identity": identity_json("a@x.io"),
                         "family": "fam-a", "issued_at": 1_000_000},
                        {"token_sha256": "a".repeat(64), "identity": identity_json("a@x.io"),
                         "family": "fam-a", "inactive_since": 1_000_500, "issued_at": 1_000_000},
                    ]
                }))
                .unwrap(),
                "duplicate token_sha256",
            ),
            (
                Subsystem::Gateway,
                "gw-version",
                br#"{"version":2,"refresh_tokens":[]}"#.to_vec(),
                "version mismatch",
            ),
            (
                Subsystem::Spend,
                "spend-json",
                b"[]".to_vec(),
                "not valid json",
            ),
            (
                Subsystem::Spend,
                "spend-version",
                br#"{"version":2,"limits":[],"audit":[],"next_audit_id":1}"#.to_vec(),
                "version mismatch",
            ),
        ];
        for (subsystem, file, bytes, reason) in cases {
            let path = put(&dir, file, &bytes);
            let error = import_all(&store, &only(subsystem, Some(&path)))
                .await
                .unwrap_err();
            let shown = error.to_string();
            assert!(matches!(error, StoreError::Import { .. }), "{error:?}");
            assert!(shown.contains(subsystem.name()), "{shown}");
            assert!(shown.contains(path.to_str().unwrap()), "{shown}");
            assert!(shown.contains(reason), "{file}: {shown}");
            assert!(record(&store, subsystem.name()).await.is_none(), "{file}");
            assert_eq!(data_rows(&store).await, 0, "{file}");
            assert_eq!(
                std::fs::read(&path).unwrap(),
                bytes,
                "{file}: snapshot untouched"
            );
            assert!(!marker::sidecar_path(&path).exists(), "{file}");
        }
    }
);

store_contract_test!(
    a_failure_after_the_rows_were_written_rolls_back_rows_and_record,
    |store| {
        let dir = TempDir::new("import-atomic");
        let pool = put(&dir, "pool.json", &v3_pool());
        let gateway = put(&dir, "sessions.json", &gateway_snapshot());
        let spend = put(&dir, "spend.json", &spend_snapshot());

        fail_record_insert(&store).await;
        for (subsystem, path) in [
            (Subsystem::Pool, &pool),
            (Subsystem::Gateway, &gateway),
            (Subsystem::Spend, &spend),
        ] {
            let error = import_all(&store, &only(subsystem, Some(path)))
                .await
                .unwrap_err();
            let shown = error.to_string();
            assert!(
                shown.contains(subsystem.name()) && shown.contains("injected"),
                "{shown}"
            );
            assert_eq!(
                data_rows(&store).await,
                0,
                "{subsystem:?} rows rolled back with the record"
            );
            assert!(!marker::sidecar_path(path).exists());
        }
        assert_eq!(count(&store, "import_record").await, 0);

        // Once the fault clears the very same boot succeeds.
        allow_record_insert(&store).await;
        let report = import_all(&store, &all_on(&pool, &gateway, &spend))
            .await
            .unwrap();
        assert!(matches!(report.pool, Outcome::Imported { .. }));
        assert!(matches!(report.gateway, Outcome::Imported { .. }));
        assert!(matches!(report.spend, Outcome::Imported { .. }));
    }
);

store_contract_test!(
    import_leaves_the_snapshot_byte_identical_and_writes_a_sidecar,
    |store| {
        let dir = TempDir::new("import-sidecar");
        let bytes = v3_pool();
        let path = put(&dir, "pool.json", &bytes);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();

        import_all(&store, &only(Subsystem::Pool, Some(&path)))
            .await
            .unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
        let sidecar = std::fs::read(marker::sidecar_path(&path)).unwrap();
        let parsed: ImportMarker = serde_json::from_slice(&sidecar).unwrap();
        assert_eq!(parsed.source_sha256, marker::sha256_hex(&bytes));
        assert_eq!(
            parsed.backend,
            crate::store::import::backend_name(store.db())
        );
        assert_eq!(
            parsed.imported_at,
            record(&store, "pool").await.unwrap().0 as u64
        );
        // Round trip through the store-less decision: unchanged bytes still match.
        assert!(marker::current_marker(&sidecar, &bytes).is_some());
        assert!(marker::current_marker(&sidecar, b"changed").is_none());
    }
);

store_contract_test!(a_sidecar_write_failure_does_not_fail_the_import, |store| {
    let dir = TempDir::new("import-sidecar-fail");
    let path = put(&dir, "pool.json", &v3_pool());
    // A directory where the sidecar file belongs makes the atomic rename fail.
    std::fs::create_dir(marker::sidecar_path(&path)).unwrap();

    let report = import_all(&store, &only(Subsystem::Pool, Some(&path)))
        .await
        .unwrap();
    assert_eq!(report.pool, Outcome::Imported { rows: 2 });
    assert!(record(&store, "pool").await.is_some());
});

#[test]
fn sources_follow_the_config_rules() {
    use crate::config::{Config, PoolConfig, SpendConfig};

    let none = ImportSources::from_config(&Config::default());
    assert_eq!(none.pool, Source::On { snapshot: None });
    assert_eq!(none.gateway, Source::Off);
    assert_eq!(none.spend, Source::Off);

    let mut config = Config::default();
    config.server.pool = Some(PoolConfig {
        state_path: Some(PathBuf::from("/s/pool.json")),
        ..Default::default()
    });
    config.server.spend = Some(SpendConfig {
        state_path: Some(PathBuf::from("/s/spend.json")),
        ..Default::default()
    });
    let sources = ImportSources::from_config(&config);
    assert_eq!(
        sources.pool,
        Source::On {
            snapshot: Some(PathBuf::from("/s/pool.json"))
        }
    );
    assert_eq!(
        sources.spend,
        Source::On {
            snapshot: Some(PathBuf::from("/s/spend.json"))
        }
    );
}
