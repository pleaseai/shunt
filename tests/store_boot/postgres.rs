//! PostgreSQL scratch schemas for the binary-level store tests.

use sqlx::Connection as _;

const URL_VAR: &str = "SHUNT_TEST_POSTGRES_URL";
const REQUIRED_VAR: &str = "SHUNT_TEST_POSTGRES_REQUIRED";

/// A `contract_<uuid>` schema for one test; the binary's connections are
/// tagged with it as `application_name` so they can be found.
pub struct Scratch {
    base: String,
    pub schema: String,
}

impl Scratch {
    /// `None` (after printing why) when the arm should be skipped; panics
    /// when the URL is required but unset.
    pub async fn new() -> Option<Self> {
        let base = {
            let _env = crate::common::env_lock().await;
            match std::env::var(URL_VAR) {
                Ok(url) if !url.trim().is_empty() => url,
                _ => {
                    assert!(
                        std::env::var(REQUIRED_VAR).as_deref() != Ok("1"),
                        "{REQUIRED_VAR}=1 but {URL_VAR} is unset"
                    );
                    println!("skipped: {URL_VAR} unset");
                    return None;
                }
            }
        };
        let schema = format!("contract_{}", uuid::Uuid::new_v4().simple());
        let scratch = Self { base, schema };
        scratch
            .admin(&format!("CREATE SCHEMA {}", scratch.schema))
            .await;
        Some(scratch)
    }

    /// The URL the binary uses: `search_path` pinned to the schema.
    pub fn url(&self) -> String {
        let mut url = url::Url::parse(&self.base).expect("postgres URL");
        url.query_pairs_mut()
            .append_pair("options", &format!("-c search_path={}", self.schema))
            .append_pair("application_name", &self.schema);
        url.to_string()
    }

    async fn connect(&self, url: &str) -> sqlx::PgConnection {
        sqlx::PgConnection::connect(url)
            .await
            .unwrap_or_else(|error| panic!("{URL_VAR} is unreachable: {error}"))
    }

    /// Run `sql` outside the schema.
    pub async fn admin(&self, sql: &str) {
        let mut conn = self.connect(&self.base).await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
            .execute(&mut conn)
            .await
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        let _ = conn.close().await;
    }

    /// Run `sql` with `search_path` pinned to the schema.
    pub async fn scoped(&self, sql: &str) {
        let mut url = url::Url::parse(&self.base).expect("postgres URL");
        url.query_pairs_mut()
            .append_pair("options", &format!("-c search_path={}", self.schema));
        let mut conn = self.connect(url.as_str()).await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
            .execute(&mut conn)
            .await
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        let _ = conn.close().await;
    }

    pub async fn count(&self, table: &str) -> i64 {
        let mut conn = self.connect(&self.base).await;
        let sql = format!("SELECT COUNT(*) FROM {}.{table}", self.schema);
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .fetch_one(&mut conn)
            .await
            .expect("count rows");
        let _ = conn.close().await;
        count
    }

    /// End every server process the binary has open.
    pub async fn terminate_binary_connections(&self) {
        self.admin(&format!(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE application_name = '{}' AND pid <> pg_backend_pid()",
            self.schema
        ))
        .await;
    }

    pub async fn drop_schema(&self) {
        let Ok(mut conn) = sqlx::PgConnection::connect(&self.base).await else {
            return;
        };
        let _ = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {} CASCADE",
            self.schema
        )))
        .execute(&mut conn)
        .await;
        let _ = conn.close().await;
    }
}
