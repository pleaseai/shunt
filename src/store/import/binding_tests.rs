//! The one-time file import carries the snapshot's session bindings
//! (ADR-0008 §6), normalized as a boot would read them.

use super::{import_all, ImportSources, Outcome, Source};
use crate::accounts::{AccountKey, AccountStateIdentity, StoreFamily};
use crate::store::contract::store_contract_test;
use crate::store::pool_session_repo;
use crate::store::test_support::TempDir;

fn key(name: &str) -> AccountKey {
    AccountKey {
        store_family: StoreFamily::Claude,
        identity: AccountStateIdentity::StoreEntry {
            name: name.to_string(),
        },
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

store_contract_test!(a_v3_snapshot_imports_its_bindings, |store| {
    let dir = TempDir::new("import-bindings");
    let live = now() - 60;
    let snapshot = serde_json::json!({
        "version": 3,
        "accounts": [],
        "bindings": [
            {"provider": "anthropic", "session_hash": "a".repeat(64),
             "account": key("a"), "last_used": live},
            // Expired: idle well past the 24 h TTL.
            {"provider": "anthropic", "session_hash": "b".repeat(64),
             "account": key("a"), "last_used": 1_000},
            // Malformed: not a SHA-256 hex.
            {"provider": "anthropic", "session_hash": "raw-session-id",
             "account": key("a"), "last_used": live},
        ]
    });
    let path = dir.file("pool.json");
    std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
    let sources = ImportSources {
        pool: Source::On {
            snapshot: Some(path),
        },
        ..ImportSources::default()
    };

    let report = import_all(&store, &sources).await.unwrap();
    assert_eq!(report.pool, Outcome::Imported { rows: 1 });
    let rows = pool_session_repo::load_bindings(&store).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].session_hash, "a".repeat(64));
    assert_eq!(rows[0].account, key("a"));
    assert_eq!(rows[0].last_used, live - live % 3_600);
});
