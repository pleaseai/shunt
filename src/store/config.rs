//! `[store]` — the optional pluggable store's configuration (ADR-0006).
//!
//! Absent (the default) means no store: every persistence choke point keeps
//! its current file-based body. When present, `backend` picks SQLite (always
//! compiled) or PostgreSQL (behind the `store-postgres` feature).
//!
//! The section is restart-only: `reload` warns when it changes (see
//! `crate::reload`), because the pool is opened once at boot.

use serde::{Deserialize, Serialize};

use crate::config::Secret;

/// Floor for `hold_stale_seconds`. The instance hold is refreshed on a
/// fraction of this window; anything shorter than a few seconds would make a
/// healthy holder look stale under an ordinary GC pause or slow commit.
pub const MIN_HOLD_STALE_SECONDS: u64 = 3;

/// Default for `hold_stale_seconds`.
pub const DEFAULT_HOLD_STALE_SECONDS: u64 = 30;

/// Which database engine backs the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StoreBackend {
    Sqlite,
    Postgres,
}

impl StoreBackend {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Postgres => "postgres",
        }
    }
}

/// The `[store]` table.
///
/// `url` is a [`Secret`] (a PostgreSQL URL commonly embeds a password), so
/// `Debug` and `Serialize` render `[redacted]`. For that reason this section
/// must stay behind `Option` in `Config` with `skip_serializing_if =
/// "Option::is_none"` (see the `Secret` invariant in `config/secrets.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoreConfig {
    pub backend: StoreBackend,
    /// SQLite: a filesystem path or a `sqlite:` URL. PostgreSQL: a
    /// `postgres://` URL.
    pub url: Secret,
    /// PostgreSQL pool size. Rejected for SQLite, which always uses exactly
    /// one connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_connections: Option<u32>,
    /// Name of an environment variable holding the PostgreSQL password. It is
    /// applied only when the URL carries no inline password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_env: Option<String>,
    /// Accept a non-loopback PostgreSQL host without `sslmode=verify-full`.
    /// Logs a warning at validation.
    #[serde(default)]
    pub allow_insecure_remote_tls: bool,
    /// Seconds without a heartbeat after which the instance hold is stale.
    #[serde(default = "default_hold_stale_seconds")]
    pub hold_stale_seconds: u64,
}

fn default_hold_stale_seconds() -> u64 {
    DEFAULT_HOLD_STALE_SECONDS
}

impl StoreConfig {
    /// Validate the section. Pure apart from one `warn!` when the TLS check is
    /// deliberately opted out of.
    pub fn validate(&self) -> Result<(), String> {
        let url = self.url.expose().trim();
        if url.is_empty() {
            return Err("store.url must not be empty".to_string());
        }
        if self.hold_stale_seconds < MIN_HOLD_STALE_SECONDS {
            return Err(format!(
                "store.hold_stale_seconds must be at least {MIN_HOLD_STALE_SECONDS}, got {}",
                self.hold_stale_seconds
            ));
        }
        match self.backend {
            StoreBackend::Sqlite => self.validate_sqlite(url),
            StoreBackend::Postgres => self.validate_postgres(url),
        }
    }

    fn validate_sqlite(&self, url: &str) -> Result<(), String> {
        if self.max_connections.is_some() {
            return Err(
                "store.max_connections applies to the postgres backend only; sqlite always uses one connection"
                    .to_string(),
            );
        }
        if sqlite_is_in_memory(url) {
            return Err(
                "store.url must name a file: an in-memory sqlite database is gone on restart and not shared across connections"
                    .to_string(),
            );
        }
        // sqlx opens every name with `SQLITE_OPEN_URI`, so SQLite would read a
        // `file:` name as a URI while the preflight (`exists`, the 0600
        // pre-create) reads it as a literal path: two different files.
        if sqlite_path(url).to_ascii_lowercase().starts_with("file:") {
            return Err(
                "store.url must be a filesystem path or a sqlite: URL; a file: URI name is not supported"
                    .to_string(),
            );
        }
        if !sqlite_split(url).1.is_empty() {
            return Err(
                "store.url query options are not supported; name the database file only"
                    .to_string(),
            );
        }
        Ok(())
    }

    #[cfg(not(feature = "store-postgres"))]
    fn validate_postgres(&self, _url: &str) -> Result<(), String> {
        Err(
            "store.backend = \"postgres\" requires a build with the `store-postgres` feature; rebuild with `--features store-postgres`"
                .to_string(),
        )
    }

    #[cfg(feature = "store-postgres")]
    fn validate_postgres(&self, url: &str) -> Result<(), String> {
        use std::str::FromStr;

        if self.max_connections == Some(0) {
            return Err("store.max_connections must be at least 1".to_string());
        }
        let options = sqlx::postgres::PgConnectOptions::from_str(url)
            .map_err(|error| format!("store.url is not a valid postgres URL: {error}"))?;
        if let Some(violation) = tls_violation(&options) {
            if !self.allow_insecure_remote_tls {
                return Err(format!(
                    "{violation}; use `sslmode=verify-full` or set store.allow_insecure_remote_tls = true"
                ));
            }
            tracing::warn!(
                store_target = %self.sanitized_target(),
                "store.allow_insecure_remote_tls is set: {violation}"
            );
        }
        Ok(())
    }

    /// Backend plus location, never credentials: `sqlite:<path>` or
    /// `postgres://host:port/db`. For errors, logs, and `shunt check`.
    pub fn sanitized_target(&self) -> String {
        let url = self.url.expose().trim();
        match self.backend {
            StoreBackend::Sqlite => format!("sqlite:{}", sqlite_path(url)),
            StoreBackend::Postgres => postgres_target(url),
        }
    }

    /// SQLite database file named by `url` (the `sqlite:` / `sqlite://` prefix
    /// and any `?query` removed, a URL's path percent-decoded). Meaningful for
    /// the SQLite backend only.
    pub(crate) fn sqlite_file(&self) -> std::path::PathBuf {
        std::path::PathBuf::from(sqlite_path(self.url.expose().trim()))
    }

    /// The `StoreBackend` label used in log fields.
    pub fn backend_label(&self) -> &'static str {
        self.backend.as_str()
    }

    /// Whether the PostgreSQL URL carries an inline password, in either the
    /// userinfo (`user:pw@host`) or the `?password=` query parameter. An empty
    /// password counts as absent.
    pub fn has_inline_password(&self) -> bool {
        url_has_inline_password(self.url.expose().trim())
    }

    /// The password `password_env` contributes, if any (AC-038): `None` when
    /// the variable is not configured or the URL carries an inline password
    /// (the variable is then not read); an error when it is configured, needed,
    /// and unset or empty, rather than a silent passwordless attempt.
    #[cfg_attr(not(feature = "store-postgres"), allow(dead_code))]
    pub(crate) fn env_password(
        &self,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<String>, String> {
        let Some(name) = self.password_env.as_deref() else {
            return Ok(None);
        };
        if self.has_inline_password() {
            return Ok(None);
        }
        match env(name).filter(|value| !value.is_empty()) {
            Some(password) => Ok(Some(password)),
            None => Err(format!(
                "store.password_env names {name}, which is unset or empty"
            )),
        }
    }

    /// PostgreSQL connect options from the URL, with `password_env` applied
    /// only when the URL has no inline password (AC-038).
    #[cfg(feature = "store-postgres")]
    pub fn pg_connect_options(&self) -> Result<sqlx::postgres::PgConnectOptions, String> {
        use std::str::FromStr;

        let mut options = sqlx::postgres::PgConnectOptions::from_str(self.url.expose().trim())
            .map_err(|error| format!("store.url is not a valid postgres URL: {error}"))?;
        if let Some(password) = self.env_password(|name| std::env::var(name).ok())? {
            options = options.password(&password);
        }
        Ok(options)
    }
}

/// The TLS rule, judged on what sqlx parsed (never the raw string). A unix
/// socket and loopback hosts need nothing; any other host needs
/// `sslmode=verify-full`.
///
/// What sqlx 0.9.0 does (`sqlx-postgres/src/options/parse.rs`): a `host` or
/// `hostaddr` query parameter overwrites the URL's host via `.host()`, so
/// `get_host()` reflects the last such parameter; a `/`-prefixed URL host or
/// `?host=` sets `socket`, and `fetch_socket` prefers `socket` over `host`.
/// A URL host of `[::1]` is reported with its brackets.
#[cfg(feature = "store-postgres")]
pub fn tls_violation(options: &sqlx::postgres::PgConnectOptions) -> Option<String> {
    use sqlx::postgres::PgSslMode;

    if options.get_socket().is_some() || options.get_host().starts_with('/') {
        return None;
    }
    let host = options.get_host();
    if is_loopback_host(host) {
        return None;
    }
    match options.get_ssl_mode() {
        PgSslMode::VerifyFull => None,
        mode => Some(format!(
            "store.url host {host:?} is not loopback and sslmode is {mode:?}, not verify-full"
        )),
    }
}

#[cfg(feature = "store-postgres")]
fn is_loopback_host(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.eq_ignore_ascii_case("localhost") {
        return true;
    }
    bare.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// Userinfo password or `password` query parameter, parsed with the `url`
/// crate. sqlx 0.9.0 has no public password getter (`PgConnectOptions` only
/// exposes `password()` as a setter), and its `new()` seeds the password from
/// `PGPASSWORD`, so the parsed options cannot tell "inline" from "ambient".
pub(crate) fn url_has_inline_password(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    parsed
        .password()
        .is_some_and(|password| !password.is_empty())
        || parsed
            .query_pairs()
            .any(|(key, value)| key == "password" && !value.is_empty())
}

fn sqlite_split(url: &str) -> (&str, &str) {
    let rest = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))
        .unwrap_or(url);
    rest.split_once('?').unwrap_or((rest, ""))
}

/// The file `url` names. A `sqlite:` URL's path is percent-decoded, as sqlx's
/// own URL parser (`sqlx-sqlite/src/options/parse.rs`) decodes it, so
/// `sqlite:///data/my%20store.db` is `/data/my store.db`; a plain path is used
/// as written, since SQLite only decodes names that start with `file:`.
fn sqlite_path(url: &str) -> String {
    let path = sqlite_split(url).0;
    if url.starts_with("sqlite:") {
        percent_decoded(path)
    } else {
        path.to_string()
    }
}

/// SQLite opens the name as a URI and percent-decodes it, so the check runs on
/// the decoded form: `file:%3Amemory%3A` is `:memory:` to SQLite.
fn sqlite_is_in_memory(url: &str) -> bool {
    let (path, query) = sqlite_split(url);
    let path = percent_decoded(path).to_ascii_lowercase();
    path.is_empty()
        || path.contains(":memory:")
        || percent_decoded(query)
            .split('&')
            .any(|pair| pair.eq_ignore_ascii_case("mode=memory"))
}

fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = (bytes[index] == b'%')
            .then(|| bytes.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn postgres_target(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return "postgres://<unparseable url>".to_string();
    };
    let mut host = parsed.host_str().unwrap_or("").to_string();
    for (key, value) in parsed.query_pairs() {
        if key == "host" || key == "hostaddr" {
            host = value.into_owned();
        }
    }
    let port = parsed
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    format!("postgres://{host}{port}{}", parsed.path())
}

#[cfg(test)]
mod tests;
