//! `shunt check` runs the routed-Antigravity credential guard, not just
//! `Config::load(...).validate()` (issue #382).
//!
//! The guard is what makes the `kind = "antigravity"` rename non-silent: a
//! migrated config that routes to the native upstream with nobody having run
//! `shunt login antigravity` must be refused. It used to live only in the
//! run/serve path, so `check` reported `config ok` for a config `run` dies on
//! — exactly backwards for the CI and deploy scripts that gate a rollout on
//! `check`.
//!
//! Every case below drives the real binary, so the credential path is set on
//! the *child* process. Writing `SHUNT_ANTIGRAVITY_AUTH_FILE` into this
//! process would instead race — and leak into — every other test sharing it.

mod common;

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "shunt-check-cli-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create temp directory");
        Self(path)
    }

    /// Write `config` as `shunt.toml` and return the path to hand `--config`.
    fn config(&self, config: &str) -> PathBuf {
        let path = self.0.join("shunt.toml");
        std::fs::write(&path, config).expect("write config");
        path
    }

    /// The credential path the child probes. Nothing is written here unless a
    /// test calls [`TempDir::credential`], so by default it does not exist.
    fn credential_path(&self) -> PathBuf {
        self.0.join("antigravity-auth.json")
    }

    fn credential(&self) -> PathBuf {
        let path = self.credential_path();
        std::fs::write(&path, "{}").expect("write credential");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn check(config: &Path, credential: &Path) -> Output {
    // `Path` is passed straight through: `Command::arg` takes `AsRef<OsStr>`,
    // so converting to `&str` first would only add a panic on a non-UTF-8
    // temp path without buying anything.
    Command::new(env!("CARGO_BIN_EXE_shunt"))
        .arg("check")
        .arg("--config")
        .arg(config)
        .env("SHUNT_ANTIGRAVITY_AUTH_FILE", credential)
        .output()
        .expect("shunt binary should run")
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).expect("stdout should be UTF-8")
}

fn stderr(output: &Output) -> &str {
    std::str::from_utf8(&output.stderr).expect("stderr should be UTF-8")
}

#[test]
fn check_refuses_a_routed_antigravity_provider_without_a_credential() {
    let dir = TempDir::new("routed-no-credential");
    let config = dir.config("[server]\ndefault_provider = \"antigravity\"\n");

    let output = check(&config, &dir.credential_path());

    assert!(
        !output.status.success(),
        "a routed antigravity provider with no credential must fail the check; \
         stdout: {}",
        stdout(&output)
    );
    assert!(
        !stdout(&output).contains("config ok"),
        "the refused config must not also be reported ok: {}",
        stdout(&output)
    );
    // The message has to be actionable on its own: which provider, what is
    // missing, and the command that fixes it.
    let message = stderr(&output);
    for expected in [
        "provider `antigravity`",
        "no credential",
        "shunt login antigravity",
    ] {
        assert!(
            message.contains(expected),
            "the failure must name {expected:?}: {message}"
        );
    }
}

#[test]
fn check_accepts_a_routed_antigravity_provider_with_a_credential() {
    let dir = TempDir::new("routed-with-credential");
    let config = dir.config("[server]\ndefault_provider = \"antigravity\"\n");

    let output = check(&config, &dir.credential());

    assert!(
        output.status.success(),
        "a credential at the probed path must satisfy the guard; stderr: {}",
        stderr(&output)
    );
    assert!(stdout(&output).contains("config ok"), "{}", stdout(&output));
}

#[test]
fn check_accepts_an_empty_credential_because_the_probe_is_presence_only() {
    // Pins a deliberate limit rather than a bug. The guard asks whether the
    // credential file exists, never what is in it, so an empty (or stale, or
    // malformed) file passes `check` and fails later on the request path.
    //
    // This test exists because the docs make that claim in prose
    // (README.md, site guides/providers.mdx). Without it the claim is
    // unfalsifiable, and a future change that started parsing the credential
    // here would silently tighten what `shunt check` accepts relative to
    // `shunt run` — reopening the divergence issue #382 closed.
    let dir = TempDir::new("empty-credential");
    let config = dir.config("[server]\ndefault_provider = \"antigravity\"\n");
    let credential = dir.credential_path();
    std::fs::write(&credential, "").expect("write empty credential");

    let output = check(&config, &credential);

    assert!(
        output.status.success(),
        "presence-only means a zero-byte credential satisfies the guard; stderr: {}",
        stderr(&output)
    );
    assert!(stdout(&output).contains("config ok"), "{}", stdout(&output));
}

#[test]
fn check_accepts_the_built_in_antigravity_provider_when_nothing_routes_to_it() {
    // Every default config seeds a built-in `antigravity` provider, so a guard
    // keyed on the provider merely *existing* would fail this — that is, every
    // config anyone has ever written — rather than on the config being able to
    // send a request to it. Keying on routing is the whole reason the run-path
    // guard is shaped the way it is; `check` inherits that, not a copy of it.
    let dir = TempDir::new("seeded-not-routed");
    let config = dir.config("[server]\ndefault_provider = \"anthropic\"\n");

    let output = check(&config, &dir.credential_path());

    assert!(
        output.status.success(),
        "a config that never routes to antigravity must pass with no credential; \
         stderr: {}",
        stderr(&output)
    );
    assert!(stdout(&output).contains("config ok"), "{}", stdout(&output));
}

#[test]
fn check_accepts_routing_to_the_antigravity_cli_transport_without_a_credential() {
    // `antigravity-cli` is the deprecated `agy` subprocess transport. It
    // authenticates through the local CLI, not through the shunt-owned
    // credential file, so the native upstream's guard must not fire on it —
    // the migration path #372 offers is precisely "route to `antigravity-cli`
    // to stay put", and a guard that refused it would close that path.
    let dir = TempDir::new("cli-transport");
    let config = dir.config("[server]\ndefault_provider = \"antigravity-cli\"\n");

    let output = check(&config, &dir.credential_path());

    assert!(
        output.status.success(),
        "antigravity_cli routing must be unaffected by the native guard; stderr: {}",
        stderr(&output)
    );
    assert!(stdout(&output).contains("config ok"), "{}", stdout(&output));
}

// --- `[store]` inspection (FR-008, AC-021, SC-005) --------------------------

use shunt::store::{Mode, Store, StoreConfig};

fn sqlite_config(dir: &TempDir, db: &Path) -> PathBuf {
    dir.config(&format!(
        "[store]\nbackend = \"sqlite\"\nurl = \"{}\"\n",
        db.display()
    ))
}

fn store_config(toml_body: &str) -> StoreConfig {
    #[derive(serde::Deserialize)]
    struct Wrapper {
        store: StoreConfig,
    }
    toml::from_str::<Wrapper>(toml_body)
        .expect("store config")
        .store
}

/// A SQLite file migrated to this binary's current schema, via the library.
async fn migrated_sqlite(db: &Path) {
    let config = store_config(&format!(
        "[store]\nbackend = \"sqlite\"\nurl = \"{}\"\n",
        db.display()
    ));
    let store = Store::open(&config, Mode::Serve).await.expect("open store");
    store.close().await;
}

fn snapshot(db: &Path) -> (Vec<u8>, SystemTime) {
    (
        std::fs::read(db).expect("read database"),
        std::fs::metadata(db)
            .and_then(|meta| meta.modified())
            .expect("database mtime"),
    )
}

fn run_check(config: &Path) -> Output {
    // The credential guard is irrelevant here; point it at nothing.
    check(config, Path::new("/nonexistent/antigravity-auth.json"))
}

#[tokio::test]
async fn check_reports_a_current_sqlite_store_without_touching_it() {
    let dir = TempDir::new("store-current");
    let db = dir.0.join("shunt.db");
    migrated_sqlite(&db).await;
    let before = snapshot(&db);
    let config = sqlite_config(&dir, &db);

    let output = run_check(&config);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let expected = format!(
        "store ok: sqlite sqlite:{}, schema v1 (binary supports v1)\n",
        db.display()
    );
    assert!(stdout(&output).contains(&expected), "{}", stdout(&output));
    assert!(stdout(&output).contains("config ok"));
    assert!(!stdout(&output).contains("pending"), "{}", stdout(&output));
    assert_eq!(snapshot(&db), before, "check must not write the database");
}

#[tokio::test]
async fn check_reports_pending_migrations_on_an_unmigrated_sqlite_file() {
    let dir = TempDir::new("store-older");
    let db = dir.0.join("shunt.db");
    std::fs::write(&db, b"").expect("create empty database file");
    let before = snapshot(&db);
    let config = sqlite_config(&dir, &db);

    let output = run_check(&config);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let expected = format!(
        "store ok: sqlite sqlite:{}, schema v0 (binary supports v1); \
         1 pending migration applies at boot\n",
        db.display()
    );
    assert!(stdout(&output).contains(&expected), "{}", stdout(&output));
    assert_eq!(snapshot(&db), before, "check must not migrate or write");
    let tables = std::fs::read(&db).expect("read database");
    assert!(
        !String::from_utf8_lossy(&tables).contains("_sqlx_migrations"),
        "check must not create the migrations table"
    );
}

#[test]
fn check_does_not_create_a_missing_sqlite_file() {
    let dir = TempDir::new("store-absent");
    let db = dir.0.join("nested-absent.db");
    let config = sqlite_config(&dir, &db);

    let output = run_check(&config);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let expected = format!(
        "store ok: sqlite sqlite:{}, file does not exist yet; it is created with mode 0600 at boot\n",
        db.display()
    );
    assert!(stdout(&output).contains(&expected), "{}", stdout(&output));
    assert!(!db.exists(), "check must not create the database file");
    for sidecar in ["-wal", "-shm"] {
        let mut name = db.clone().into_os_string();
        name.push(sidecar);
        assert!(!Path::new(&name).exists(), "no {sidecar} sidecar either");
    }
}

#[tokio::test]
async fn check_refuses_a_sqlite_store_from_a_newer_binary() {
    let dir = TempDir::new("store-too-new");
    let db = dir.0.join("shunt.db");
    migrated_sqlite(&db).await;
    {
        use sqlx::Connection as _;
        let mut conn = sqlx::SqliteConnection::connect(&format!("sqlite:{}", db.display()))
            .await
            .expect("open for fixture");
        sqlx::raw_sql(
            "INSERT INTO _sqlx_migrations \
             (version, description, success, checksum, execution_time) \
             VALUES (999, 'from the future', 1, x'00', 0)",
        )
        .execute(&mut conn)
        .await
        .expect("insert newer migration");
        let _ = conn.close().await;
    }
    let before = snapshot(&db);
    let config = sqlite_config(&dir, &db);

    let output = run_check(&config);

    assert!(!output.status.success(), "stdout: {}", stdout(&output));
    let message = stderr(&output);
    for expected in ["schema version 999", "newer than the 1", "upgrade shunt"] {
        assert!(message.contains(expected), "{expected:?} in: {message}");
    }
    assert!(!stdout(&output).contains("store ok"), "{}", stdout(&output));
    assert_eq!(snapshot(&db), before, "check must not write the database");
}

#[cfg(feature = "store-postgres")]
mod postgres {
    use super::*;
    use sqlx::Connection as _;

    const URL_VAR: &str = "SHUNT_TEST_POSTGRES_URL";
    const REQUIRED_VAR: &str = "SHUNT_TEST_POSTGRES_REQUIRED";

    /// The base URL, or `None` after printing why the arm is skipped; panics
    /// when the URL is required but unset.
    fn base_url() -> Option<String> {
        match std::env::var(URL_VAR) {
            Ok(url) if !url.trim().is_empty() => Some(url),
            _ => {
                assert!(
                    std::env::var(REQUIRED_VAR).as_deref() != Ok("1"),
                    "{REQUIRED_VAR}=1 but {URL_VAR} is unset"
                );
                println!("skipped: {URL_VAR} unset");
                None
            }
        }
    }

    async fn admin(base: &str, sql: String) {
        let mut conn = sqlx::PgConnection::connect(base)
            .await
            .unwrap_or_else(|error| panic!("{URL_VAR} is unreachable: {error}"));
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&mut conn)
            .await
            .expect("admin statement");
        let _ = conn.close().await;
    }

    fn scoped_url(base: &str, schema: &str) -> String {
        let mut url = url::Url::parse(base).expect("postgres URL");
        url.query_pairs_mut()
            .append_pair("options", &format!("-c search_path={schema}"));
        url.to_string()
    }

    fn config_for(dir: &TempDir, url: &str) -> PathBuf {
        dir.config(&format!(
            "[store]\nbackend = \"postgres\"\nurl = \"{url}\"\n"
        ))
    }

    #[tokio::test]
    async fn check_reports_a_reachable_postgres_store_without_the_password() {
        // Held for the whole body: `base_url` reads the environment and the
        // child inherits it.
        let _env = common::env_lock().await;
        let Some(base) = base_url() else { return };
        // The password the configured URL really carries, in the userinfo or
        // the `password` query parameter. `PGPASSWORD` or passwordless local
        // auth leaves none, and then there is nothing in the URL to leak.
        let parsed = url::Url::parse(&base).expect("postgres URL");
        let password = parsed
            .password()
            .map(str::to_owned)
            .or_else(|| {
                parsed
                    .query_pairs()
                    .find(|(key, _)| key == "password")
                    .map(|(_, value)| value.into_owned())
            })
            .filter(|password| !password.is_empty());
        let schema = format!("check_{}", uuid::Uuid::new_v4().simple());
        admin(&base, format!("CREATE SCHEMA {schema}")).await;
        let url = scoped_url(&base, &schema);
        let dir = TempDir::new("store-pg-ok");
        let config = config_for(&dir, &url);

        let output = run_check(&config);
        // The schema is empty: nothing was migrated and `check` must not have.
        let created: i64 = {
            let mut conn = sqlx::PgConnection::connect(&base).await.expect("connect");
            let count = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = '{schema}'"
            )))
            .fetch_one(&mut conn)
            .await
            .expect("count tables");
            let _ = conn.close().await;
            count
        };
        admin(&base, format!("DROP SCHEMA {schema} CASCADE")).await;

        assert!(output.status.success(), "stderr: {}", stderr(&output));
        let line = stdout(&output)
            .lines()
            .find(|line| line.starts_with("store ok: postgres postgres://"))
            .unwrap_or_else(|| panic!("no store line in: {}", stdout(&output)));
        assert!(
            line.ends_with(", schema v0 (binary supports v1); 1 pending migration applies at boot"),
            "{line}"
        );
        assert_eq!(created, 0, "check must not create any table");
        let Some(password) = password else {
            eprintln!("SHUNT_TEST_POSTGRES_URL carries no inline password; redaction not checked");
            return;
        };
        for stream in [stdout(&output), stderr(&output)] {
            assert!(!stream.contains(&password), "password leaked: {stream}");
        }
    }

    #[test]
    fn check_fails_on_an_unreachable_postgres_store_without_the_password() {
        let _env = common::set_env_blocking(&[]);
        let dir = TempDir::new("store-pg-down");
        let config = config_for(
            &dir,
            "postgres://shunt:hunter2-unreachable@127.0.0.1:1/shunt?sslmode=disable",
        );

        let output = run_check(&config);

        assert!(!output.status.success(), "stdout: {}", stdout(&output));
        assert!(
            stderr(&output).contains("127.0.0.1:1"),
            "{}",
            stderr(&output)
        );
        assert!(!stdout(&output).contains("store ok"));
        for stream in [stdout(&output), stderr(&output)] {
            assert!(
                !stream.contains("hunter2-unreachable"),
                "password leaked: {stream}"
            );
        }
    }
}
