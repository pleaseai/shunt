use std::path::Path;

use sqlx::migrate::Migrator;

use super::*;
use crate::store::test_support::{sqlite_config, TempDir};

fn dir_snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect();
    entries.sort();
    entries
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
#[tokio::test]
async fn new_file_is_0600_and_sidecars_follow_after_a_write() {
    let dir = TempDir::new("perm");
    let path = dir.file("s.db");
    let store = Store::open(&sqlite_config(&path), Mode::Serve)
        .await
        .unwrap();
    assert_eq!(mode_of(&path), 0o600);

    // Write, and keep the connection open so -wal and -shm exist.
    let mut tx = store.db().begin().await.unwrap();
    tx.execute(
        "INSERT INTO import_record (subsystem, imported_at, source_path, source_sha256, row_count) \
         VALUES ($1, $2, NULL, NULL, $3)",
        &["pool".into(), 1_i64.into(), 0_i64.into()],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    for suffix in ["-wal", "-shm"] {
        let sidecar = dir.file(&format!("s.db{suffix}"));
        assert!(sidecar.exists(), "{suffix} should exist while open");
        assert_eq!(mode_of(&sidecar), 0o600, "{suffix} mode");
    }
    store.close().await;
}

#[tokio::test]
async fn pending_migrations_apply_before_open_returns() {
    let dir = TempDir::new("migrate");
    let config = sqlite_config(&dir.file("s.db"));
    let store = Store::open(&config, Mode::Serve).await.unwrap();
    assert_eq!(store.schema_version().await.unwrap(), 1);
    assert_eq!(store.supported_version(), 1);
    assert_eq!(store.pending_migrations().await.unwrap(), 0);
    store.release_hold().await.unwrap();
    store.close().await;

    // An existing, already-migrated file reopens without error once the
    // previous holder released the instance hold.
    let again = Store::open(&config, Mode::Serve).await.unwrap();
    assert_eq!(again.schema_version().await.unwrap(), 1);
    again.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn existing_file_keeps_its_mode() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("existing");
    let path = dir.file("s.db");
    std::fs::write(&path, b"").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    let store = Store::open(&sqlite_config(&path), Mode::Serve)
        .await
        .unwrap();
    assert_eq!(mode_of(&path), 0o640);
    store.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn unwritable_directory_is_unreachable_and_names_the_target() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("unwritable");
    let locked = dir.file("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    if std::fs::File::create(locked.join("probe")).is_ok() {
        // Running as root: permission bits do not bind, nothing to prove.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    let config = sqlite_config(&locked.join("s.db"));
    let error = Store::open(&config, Mode::Serve).await.unwrap_err();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(error, StoreError::Unreachable { .. }), "{error:?}");
    let shown = error.to_string();
    assert!(shown.contains("unreachable"), "{shown}");
    assert!(shown.contains(&config.sanitized_target()), "{shown}");
}

#[tokio::test]
async fn a_failing_migration_returns_migration_error() {
    let dir = TempDir::new("badmig");
    let migrations = dir.file("migrations");
    std::fs::create_dir(&migrations).unwrap();
    std::fs::write(migrations.join("0001_bad.sql"), "THIS IS NOT SQL;").unwrap();
    let migrator: &'static Migrator =
        Box::leak(Box::new(Migrator::new(migrations.as_path()).await.unwrap()));

    let config = sqlite_config(&dir.file("s.db"));
    let error = Store::open_with(&config, Mode::Serve, migrator)
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::Migration { .. }), "{error:?}");
    let shown = error.to_string();
    assert!(shown.contains("migration failed"), "{shown}");
    assert!(shown.contains(&config.sanitized_target()), "{shown}");
}

#[tokio::test]
async fn a_failed_migration_releases_the_hold_for_a_corrected_restart() {
    let dir = TempDir::new("badmig-hold");
    let migrations = dir.file("migrations");
    std::fs::create_dir(&migrations).unwrap();
    std::fs::write(migrations.join("0001_bad.sql"), "THIS IS NOT SQL;").unwrap();
    let bad: &'static Migrator =
        Box::leak(Box::new(Migrator::new(migrations.as_path()).await.unwrap()));

    let config = sqlite_config(&dir.file("s.db"));
    let error = Store::open_with(&config, Mode::Serve, bad)
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::Migration { .. }), "{error:?}");

    let store = Store::open(&config, Mode::Serve)
        .await
        .expect("the failed open released its hold");
    store.close().await;
}

#[tokio::test]
async fn too_new_schema_is_refused_and_the_file_is_untouched() {
    let dir = TempDir::new("toonew");
    let data = dir.0.join("data");
    std::fs::create_dir(&data).unwrap();
    let path = data.join("s.db");
    let config = sqlite_config(&path);

    let store = Store::open(&config, Mode::Serve).await.unwrap();
    let mut tx = store.db().begin().await.unwrap();
    tx.execute(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES ($1, 'from the future', 1, $2, 0)",
        &[9999_i64.into(), "00".into()],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    store.close().await;

    let before = dir_snapshot(&data);
    let error = Store::open(&config, Mode::Serve).await.unwrap_err();
    assert!(
        matches!(
            error,
            StoreError::SchemaTooNew {
                found: 9999,
                supported: 1,
                ..
            }
        ),
        "{error:?}"
    );
    assert!(error.to_string().contains("9999"));
    assert_eq!(dir_snapshot(&data), before);
}

#[tokio::test]
async fn inspect_reports_absent_without_creating_the_file() {
    let dir = TempDir::new("absent");
    let path = dir.file("s.db");
    let error = Store::open(&sqlite_config(&path), Mode::Inspect)
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::Absent { .. }), "{error:?}");
    assert!(!path.exists());
}

#[tokio::test]
async fn inspect_reads_versions_and_never_modifies_the_database_file() {
    let dir = TempDir::new("inspect");
    let data = dir.0.join("data");
    std::fs::create_dir(&data).unwrap();
    let path = data.join("s.db");
    let config = sqlite_config(&path);
    Store::open(&config, Mode::Serve)
        .await
        .unwrap()
        .close()
        .await;

    // SQLite may create or drop -shm/-wal sidecars for a WAL database even
    // read-only, so the contract asserted is that the database file itself is
    // unchanged; sidecar files are deliberately not compared.
    let before = std::fs::read(&path).unwrap();
    let store = Store::open(&config, Mode::Inspect).await.unwrap();
    assert_eq!(store.check_schema().await.unwrap(), 1);
    assert_eq!(store.pending_migrations().await.unwrap(), 0);
    let mut tx = store.db().begin().await.unwrap();
    assert!(tx.execute("DELETE FROM pool_quota", &[]).await.is_err());
    tx.rollback().await.unwrap();
    store.close().await;
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn inspect_of_an_unmigrated_file_sees_version_zero() {
    let dir = TempDir::new("empty");
    let path = dir.file("s.db");
    std::fs::write(&path, b"").unwrap();
    let store = Store::open(&sqlite_config(&path), Mode::Inspect)
        .await
        .unwrap();
    assert_eq!(store.schema_version().await.unwrap(), 0);
    assert_eq!(store.pending_migrations().await.unwrap(), 1);
    store.close().await;
}

#[tokio::test]
async fn unreachable_postgres_error_never_shows_the_password() {
    let mut config = sqlite_config(Path::new("unused"));
    config.backend = StoreBackend::Postgres;
    // Assembled at runtime so no connection-string literal carries a password.
    let password = "hunter2-secret";
    config.url = ["postgres://shunt:", password, "@127.0.0.1:1/shunt"]
        .concat()
        .into();
    let error = Store::open(&config, Mode::Serve).await.unwrap_err();
    assert!(matches!(error, StoreError::Unreachable { .. }), "{error:?}");
    let shown = error.to_string();
    assert!(!shown.contains(password), "{shown}");
    assert!(shown.contains("127.0.0.1:1"), "{shown}");
}
