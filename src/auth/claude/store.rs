//! Shunt-owned Claude account files.
//!
//! Each account is stored as a Claude Code `.credentials.json`-shaped file at
//! `~/.shunt/accounts/claude/<name>.json` (or
//! `$SHUNT_CLAUDE_ACCOUNTS_DIR/<name>.json`). This keeps the existing
//! [`super::auth::ClaudeAuthStore`] as the single reader/refresher for
//! both imported refreshable logins and long-lived setup tokens.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};

use crate::auth::shared;
use crate::config::AccountConfig;

const SETUP_TOKEN_LIFETIME: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// `claudeAiOauth.shuntCredentialKind` value marking a long-lived, non-refreshable
/// setup token. Written here by [`store_setup_token`] and read back by
/// `account_is_static_store_token` in the Anthropic adapter — shared so the two
/// sides cannot silently drift.
pub(crate) const SETUP_TOKEN_KIND: &str = "setup_token";

// Name validation, directory resolution, scan, and born-private write are
// identical to the Codex store, so they live in `auth::shared` and both stores
// call them — only the env var and subdir differ here.
pub use crate::auth::shared::validate_account_name;

pub fn default_accounts_dir() -> PathBuf {
    shared::default_accounts_dir("SHUNT_CLAUDE_ACCOUNTS_DIR", "claude")
}

pub fn account_path(name: &str) -> PathBuf {
    default_accounts_dir().join(format!("{name}.json"))
}

/// Return store-managed accounts in deterministic name order. Each entry's UUID
/// is read from its stored credential file, unlike the Codex store.
pub fn scan_accounts() -> io::Result<Vec<AccountConfig>> {
    shared::scan_account_dir(&default_accounts_dir(), read_account_uuid)
}

pub fn account_uuid(name: &str) -> Option<String> {
    read_account_uuid(&account_path(name))
}

/// Whether a store account holds a long-lived setup token or an imported,
/// refreshable Claude Code login. Reported by the admin surface; never carries
/// token material.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    SetupToken,
    Imported,
}

/// Token-free account metadata for the admin surface: name, kind, expiry, and
/// UUID read from the store JSON. The access/refresh token is never read here.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountMeta {
    pub name: String,
    pub kind: AccountKind,
    /// `claudeAiOauth.expiresAt` in Unix epoch milliseconds, when present.
    pub expires_at: Option<i64>,
    pub uuid: Option<String>,
}

/// Read one store account's metadata without loading its token. `None` when the
/// file is missing or unparseable.
pub fn account_meta(name: &str) -> Option<AccountMeta> {
    read_account_meta(name, &account_path(name))
}

/// List store-managed accounts with token-free metadata, in deterministic name
/// order. Mirrors [`scan_accounts`] but reports kind/expiry for the dashboard.
pub fn list_account_meta() -> io::Result<Vec<AccountMeta>> {
    Ok(scan_accounts()?
        .into_iter()
        .filter_map(|account| account_meta(&account.name))
        .collect())
}

fn read_account_meta(name: &str, path: &Path) -> Option<AccountMeta> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(account = %name, %error, "admin: failed to read account file; omitting from dashboard");
            return None;
        }
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(account = %name, %error, "admin: account file is not valid JSON; omitting from dashboard");
            return None;
        }
    };
    let oauth = value.get("claudeAiOauth");
    // Kind is decided by the refresh token: an imported login always carries a
    // non-empty `refreshToken`, while a setup-token file has none. (Setup-token
    // files also write `shuntCredentialKind`, but its absence is not relied on
    // here — refresh-token presence is the single sufficient signal.)
    let is_imported = oauth
        .and_then(|oauth| oauth.get("refreshToken"))
        .and_then(Value::as_str)
        .is_some_and(|token| !token.is_empty());
    let kind = if is_imported {
        AccountKind::Imported
    } else {
        AccountKind::SetupToken
    };
    let expires_at = oauth
        .and_then(|oauth| oauth.get("expiresAt"))
        .and_then(Value::as_i64);
    let uuid = value
        .get("shuntAccountUuid")
        .and_then(Value::as_str)
        .filter(|uuid| !uuid.is_empty())
        .map(ToOwned::to_owned);
    Some(AccountMeta {
        name: name.to_string(),
        kind,
        expires_at,
        uuid,
    })
}

/// Remove a store account file. Returns whether a file was actually removed
/// (`false` when it did not exist). The name is validated so a caller-supplied
/// value can never escape the accounts directory. This deletes an operator-owned
/// setup-token/import file only; it never touches the account's upstream state.
pub fn remove_account(name: &str) -> anyhow::Result<bool> {
    validate_account_name(name)?;
    match fs::remove_file(account_path(name)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn read_account_uuid(path: &Path) -> Option<String> {
    let value: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    value
        .get("shuntAccountUuid")
        .and_then(Value::as_str)
        .filter(|uuid| !uuid.is_empty())
        .map(ToOwned::to_owned)
}

/// Import a refreshable Claude Code credential file without changing the source.
pub fn import_credentials(
    name: &str,
    source: &Path,
    account_uuid: Option<&str>,
) -> anyhow::Result<PathBuf> {
    validate_account_name(name)?;
    let mut value: Value = serde_json::from_slice(&fs::read(source)?).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Claude credentials JSON: {error}"),
        )
    })?;
    let oauth = value.get("claudeAiOauth");
    if oauth
        .and_then(|oauth| oauth.get("accessToken"))
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .is_none()
        || oauth
            .and_then(|oauth| oauth.get("refreshToken"))
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .is_none()
    {
        anyhow::bail!(
            "{} does not contain refreshable claudeAiOauth credentials",
            source.display()
        );
    }
    if let Some(account_uuid) = account_uuid.filter(|uuid| !uuid.is_empty()) {
        value
            .as_object_mut()
            .expect("validated credential file is a JSON object")
            .insert("shuntAccountUuid".to_string(), json!(account_uuid));
    }
    write_account(name, &value)
}

/// Store a one-year token in the shape consumed by `ClaudeAuthStore`.
pub fn store_setup_token(
    name: &str,
    token: &str,
    account_uuid: Option<&str>,
) -> anyhow::Result<PathBuf> {
    validate_account_name(name)?;
    let token = token.trim();
    if token.is_empty() || token.chars().any(char::is_whitespace) {
        anyhow::bail!("setup token must be one non-empty value without whitespace");
    }
    let expires_at = SystemTime::now()
        .checked_add(SETUP_TOKEN_LIFETIME)
        .unwrap_or(SystemTime::now())
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let mut value = json!({
        "claudeAiOauth": {
            "accessToken": token,
            "expiresAt": expires_at,
            "shuntCredentialKind": SETUP_TOKEN_KIND
        }
    });
    if let Some(account_uuid) = account_uuid.filter(|uuid| !uuid.is_empty()) {
        value
            .as_object_mut()
            .expect("setup-token credential is a JSON object")
            .insert("shuntAccountUuid".to_string(), json!(account_uuid));
    }
    write_account(name, &value)
}

/// Store a freshly issued refreshable OAuth login (access + refresh token) in the
/// shape `ClaudeAuthStore` reads and auto-refreshes — identical to what
/// [`import_credentials`] produces, so `read_account_meta` reports it as
/// `Imported` and the pool treats it as refreshable. Unlike [`store_setup_token`]
/// this writes a non-empty `refreshToken` and no `shuntCredentialKind` marker.
pub fn store_oauth_tokens(
    name: &str,
    access_token: &str,
    refresh_token: &str,
    expires_at_ms: i64,
    account_uuid: Option<&str>,
) -> anyhow::Result<PathBuf> {
    validate_account_name(name)?;
    let access_token = access_token.trim();
    let refresh_token = refresh_token.trim();
    if access_token.is_empty() || access_token.chars().any(char::is_whitespace) {
        anyhow::bail!("OAuth access token must be one non-empty value without whitespace");
    }
    if refresh_token.is_empty() || refresh_token.chars().any(char::is_whitespace) {
        anyhow::bail!("OAuth refresh token must be one non-empty value without whitespace");
    }
    let mut value = json!({
        "claudeAiOauth": {
            "accessToken": access_token,
            "refreshToken": refresh_token,
            "expiresAt": expires_at_ms
        }
    });
    if let Some(account_uuid) = account_uuid.filter(|uuid| !uuid.is_empty()) {
        value
            .as_object_mut()
            .expect("oauth-login credential is a JSON object")
            .insert("shuntAccountUuid".to_string(), json!(account_uuid));
    }
    write_account(name, &value)
}

fn write_account(name: &str, value: &Value) -> anyhow::Result<PathBuf> {
    let path = account_path(name);
    shared::write_account_file(&path, value)?;
    Ok(path)
}

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shunt-claude-store-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn validates_account_names() {
        assert!(validate_account_name("primary-2").is_ok());
        for invalid in ["", "Primary", "has space", "../escape", "under_score"] {
            assert!(
                validate_account_name(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
    }

    #[test]
    fn read_account_meta_classifies_kind_and_skips_bad_files() {
        let dir = temp_dir("meta");
        fs::create_dir_all(&dir).unwrap();

        // A non-empty refreshToken marks an imported login.
        let imported = dir.join("imp.json");
        fs::write(
            &imported,
            r#"{"claudeAiOauth":{"refreshToken":"r","expiresAt":123},"shuntAccountUuid":"u1"}"#,
        )
        .unwrap();
        let meta = read_account_meta("imp", &imported).expect("imported meta parses");
        assert!(matches!(meta.kind, AccountKind::Imported));
        assert_eq!(meta.expires_at, Some(123));
        assert_eq!(meta.uuid.as_deref(), Some("u1"));

        // No refreshToken ⇒ a static setup-token file.
        let setup = dir.join("set.json");
        fs::write(&setup, r#"{"claudeAiOauth":{"expiresAt":456}}"#).unwrap();
        let meta = read_account_meta("set", &setup).expect("setup meta parses");
        assert!(matches!(meta.kind, AccountKind::SetupToken));

        // Corrupt JSON and a missing file are both omitted (None), not errors.
        let bad = dir.join("bad.json");
        fs::write(&bad, "not json").unwrap();
        assert!(read_account_meta("bad", &bad).is_none());
        assert!(read_account_meta("missing", &dir.join("nope.json")).is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn setup_token_round_trips_and_replaces() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("setup");
        // Declared after TEST_ENV_LOCK so it drops first: the var is removed on
        // drop (panic-safe) while the lock is still held.
        let _env = shared::EnvVarGuard::set("SHUNT_CLAUDE_ACCOUNTS_DIR", &dir);

        let path = store_setup_token("ci", "token-one", Some("uuid-one")).unwrap();
        store_setup_token("ci", "token-two", Some("uuid-two")).unwrap();
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["claudeAiOauth"]["accessToken"], "token-two");
        assert_eq!(value["claudeAiOauth"]["shuntCredentialKind"], "setup_token");
        assert_eq!(value["shuntAccountUuid"], "uuid-two");
        assert!(value["claudeAiOauth"]["expiresAt"].as_i64().unwrap() > 0);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn oauth_tokens_round_trip_as_refreshable_account() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("oauth");
        let _env = shared::EnvVarGuard::set("SHUNT_CLAUDE_ACCOUNTS_DIR", &dir);

        let path = store_oauth_tokens(
            "refreshable",
            "access-token",
            "refresh-token",
            4_000_000_000_000,
            Some("uuid-oauth"),
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["claudeAiOauth"]["refreshToken"], "refresh-token");
        assert_eq!(value["claudeAiOauth"]["expiresAt"], 4_000_000_000_000_i64);
        assert_eq!(value["shuntAccountUuid"], "uuid-oauth");
        assert!(value["claudeAiOauth"].get("shuntCredentialKind").is_none());
        let meta = read_account_meta("refreshable", &path).expect("OAuth meta parses");
        assert!(matches!(meta.kind, AccountKind::Imported));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn imports_and_scans_refreshable_accounts_in_name_order() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("import");
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.json");
        fs::write(
            &source,
            r#"{"claudeAiOauth":{"accessToken":"access","refreshToken":"refresh","expiresAt":4000000000000}}"#,
        )
        .unwrap();
        let accounts_dir = dir.join("accounts");
        let _env = shared::EnvVarGuard::set("SHUNT_CLAUDE_ACCOUNTS_DIR", &accounts_dir);

        import_credentials("zeta", &source, Some("uuid-zeta")).unwrap();
        import_credentials("alpha", &source, Some("uuid-alpha")).unwrap();
        fs::write(accounts_dir.join("ignore.txt"), "not an account").unwrap();
        let accounts = scan_accounts().unwrap();
        assert_eq!(accounts[0].name, "alpha");
        assert_eq!(accounts[0].uuid.as_deref(), Some("uuid-alpha"));
        assert_eq!(accounts[1].name, "zeta");
        assert_eq!(accounts[1].uuid.as_deref(), Some("uuid-zeta"));

        let _ = fs::remove_dir_all(dir);
    }
}
