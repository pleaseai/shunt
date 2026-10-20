//! The cross-engine contract harness: one test body, run once per engine.
//!
//! ```ignore
//! store_contract_test!(my_case, |store| {
//!     let mut tx = store.db().begin().await.unwrap();
//!     // ... exercise `tx`; the body runs against SQLite and PostgreSQL ...
//!     tx.commit().await.unwrap();
//! });
//! ```
//!
//! The macro expands to `mod my_case { sqlite, postgres }`. The SQLite arm
//! uses a [`TempDir`] file. The PostgreSQL arm (compiled only with
//! `store-postgres`) reads `SHUNT_TEST_POSTGRES_URL`, creates a fresh
//! `contract_<random>` schema, opens the store with `search_path` pinned to it
//! (so migrations and `_sqlx_migrations` land there), and drops the schema
//! afterwards. With the URL unset the arm prints one `skipped` line and
//! passes, unless `SHUNT_TEST_POSTGRES_REQUIRED=1`, which makes it fail.

use super::open::{Mode, Store};
use super::test_support::{sqlite_config, TempDir};

/// An open store plus whatever must be torn down after the test body.
pub(crate) struct Harness {
    pub(crate) store: Store,
    _dir: Option<TempDir>,
    #[cfg(feature = "store-postgres")]
    postgres: Option<postgres::Scratch>,
}

impl Harness {
    /// A migrated SQLite store in a fresh temp directory.
    pub(crate) async fn sqlite() -> Harness {
        let dir = TempDir::new("contract");
        let store = Store::open(&sqlite_config(&dir.file("store.db")), Mode::Serve)
            .await
            .expect("open sqlite store");
        Harness {
            store,
            _dir: Some(dir),
            #[cfg(feature = "store-postgres")]
            postgres: None,
        }
    }

    /// Close the store and drop any scratch schema (best effort).
    pub(crate) async fn finish(self) {
        self.store.close().await;
        #[cfg(feature = "store-postgres")]
        if let Some(scratch) = self.postgres {
            scratch.drop_schema().await;
        }
    }
}

#[cfg(feature = "store-postgres")]
pub(crate) mod postgres {
    use sqlx::Connection as _;

    use super::{Harness, Mode, Store};
    use crate::store::config::{StoreBackend, StoreConfig, DEFAULT_HOLD_STALE_SECONDS};

    const URL_VAR: &str = "SHUNT_TEST_POSTGRES_URL";
    const REQUIRED_VAR: &str = "SHUNT_TEST_POSTGRES_REQUIRED";

    /// A schema created for one test.
    pub(super) struct Scratch {
        admin_url: String,
        schema: String,
    }

    impl Scratch {
        pub(super) async fn drop_schema(self) {
            let Ok(mut conn) = sqlx::PgConnection::connect(&self.admin_url).await else {
                return;
            };
            let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                self.schema
            )))
            .execute(&mut conn)
            .await;
            let _ = conn.close().await;
        }
    }

    /// The configured test URL, `None` (after printing why) when the arm
    /// should be skipped. Panics when the URL is required but unset.
    ///
    /// Reads under `CONFIG_ENV_LOCK` so it cannot race the tests that write
    /// the environment under it: `setenv` may reallocate `environ` beneath an
    /// unlocked reader. The guard drops on return, before `harness` awaits.
    fn test_url() -> Option<String> {
        read_test_url(URL_VAR, REQUIRED_VAR)
    }

    /// [`test_url`] over caller-chosen variable names, so a test can drive
    /// the read through variables no other test uses.
    pub(super) fn read_test_url(url_var: &str, required_var: &str) -> Option<String> {
        let _lock = crate::config::CONFIG_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match std::env::var(url_var) {
            Ok(url) if !url.trim().is_empty() => Some(url),
            _ => {
                assert!(
                    std::env::var(required_var).as_deref() != Ok("1"),
                    "{required_var}=1 but {url_var} is unset"
                );
                println!("skipped: {url_var} unset");
                None
            }
        }
    }

    /// `base` with `search_path` pinned to `schema` for every connection.
    fn scoped_url(base: &str, schema: &str) -> String {
        let mut url = url::Url::parse(base).expect("SHUNT_TEST_POSTGRES_URL is a URL");
        url.query_pairs_mut()
            .append_pair("options", &format!("-c search_path={schema}"));
        url.to_string()
    }

    /// A migrated store in a fresh schema, or `None` when skipped.
    pub(crate) async fn harness() -> Option<Harness> {
        let base = test_url()?;
        let schema = format!("contract_{}", uuid::Uuid::new_v4().simple());
        let mut admin = sqlx::PgConnection::connect(&base)
            .await
            .unwrap_or_else(|error| panic!("{URL_VAR} is unreachable: {error}"));
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&mut admin)
            .await
            .expect("create scratch schema");
        let _ = admin.close().await;
        let config = StoreConfig {
            backend: StoreBackend::Postgres,
            url: scoped_url(&base, &schema).as_str().into(),
            max_connections: None,
            password_env: None,
            allow_insecure_remote_tls: true,
            hold_stale_seconds: DEFAULT_HOLD_STALE_SECONDS,
        };
        let scratch = Scratch {
            admin_url: base,
            schema,
        };
        let store = match Store::open(&config, Mode::Serve).await {
            Ok(store) => store,
            Err(error) => {
                scratch.drop_schema().await;
                panic!("open postgres store: {error}");
            }
        };
        Some(Harness {
            store,
            _dir: None,
            postgres: Some(scratch),
        })
    }
}

/// Define a test that runs `$body` once per engine with `$store: Store`.
macro_rules! store_contract_test {
    ($name:ident, |$store:ident| $body:block) => {
        mod $name {
            #[allow(unused_imports)]
            use super::*;

            #[tokio::test]
            async fn sqlite() {
                let harness = $crate::store::contract::Harness::sqlite().await;
                let $store = harness.store.clone();
                $body
                harness.finish().await;
            }

            #[cfg(feature = "store-postgres")]
            #[tokio::test]
            async fn postgres() {
                let Some(harness) = $crate::store::contract::postgres::harness().await else {
                    return;
                };
                let $store = harness.store.clone();
                // A panicking body must still drop its scratch schema.
                let outcome = ::futures_util::FutureExt::catch_unwind(
                    ::std::panic::AssertUnwindSafe(async move { $body }),
                )
                .await;
                harness.finish().await;
                if let Err(payload) = outcome {
                    ::std::panic::resume_unwind(payload);
                }
            }
        }
    };
}
#[allow(unused_imports)] // first path-importer arrives with the repo tasks (T005+)
pub(crate) use store_contract_test;

#[cfg(test)]
mod tests;
