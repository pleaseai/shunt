//! Shunt-owned Claude account files.
//!
//! Each account is stored as a Claude Code `.credentials.json`-shaped file at
//! `~/.shunt/accounts/claude/<name>.json` (or
//! `$SHUNT_CLAUDE_ACCOUNTS_DIR/<name>.json`). This keeps the existing
//! [`super::claude_auth::ClaudeAuthStore`] as the single reader/refresher for
//! both imported refreshable logins and long-lived setup tokens.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};

use crate::auth::shared::write_auth_file_atomic;
use crate::config::AccountConfig;

const SETUP_TOKEN_LIFETIME: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// `claudeAiOauth.shuntCredentialKind` value marking a long-lived, non-refreshable
/// setup token. Written here by [`store_setup_token`] and read back by
/// `account_is_static_store_token` in the Anthropic adapter — shared so the two
/// sides cannot silently drift.
pub(crate) const SETUP_TOKEN_KIND: &str = "setup_token";

pub fn default_accounts_dir() -> PathBuf {
    env::var_os("SHUNT_CLAUDE_ACCOUNTS_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            // `HOME` is unset on Windows; fall back to `USERPROFILE` so the store
            // lands in the user's home rather than a working-directory-relative
            // path (mirrors `default_cursor_auth_path` in auth/mod.rs).
            env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .or_else(|| env::var_os("USERPROFILE").filter(|home| !home.is_empty()))
                .map(PathBuf::from)
                .map(|home| home.join(".shunt").join("accounts").join("claude"))
        })
        .unwrap_or_else(|| PathBuf::from(".shunt/accounts/claude"))
}

pub fn account_path(name: &str) -> PathBuf {
    default_accounts_dir().join(format!("{name}.json"))
}

pub fn validate_account_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        anyhow::bail!("account name {name:?} must match [a-z0-9-]+");
    }
    Ok(())
}

/// Return store-managed accounts in deterministic name order.
pub fn scan_accounts() -> io::Result<Vec<AccountConfig>> {
    let dir = default_accounts_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut accounts = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|name| name.to_str()) else {
            continue;
        };
        if validate_account_name(name).is_err() {
            continue;
        }
        accounts.push(AccountConfig {
            name: name.to_string(),
            credentials: None,
            token_env: None,
            uuid: read_account_uuid(&path),
        });
    }
    accounts.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(accounts)
}

pub fn account_uuid(name: &str) -> Option<String> {
    read_account_uuid(&account_path(name))
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

fn write_account(name: &str, value: &Value) -> anyhow::Result<PathBuf> {
    let path = account_path(name);
    if let Some(parent) = path.parent() {
        // Create the account directory born-private (0700 on Unix) rather than
        // chmod-ing after creation, so there is no window where it sits at the
        // umask default on a multi-user host.
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent)?;
    }
    write_auth_file_atomic(&path, value)?;
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

    #[tokio::test]
    async fn setup_token_round_trips_and_replaces() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("setup");
        std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &dir);

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

        std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
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
        std::env::set_var("SHUNT_CLAUDE_ACCOUNTS_DIR", &accounts_dir);

        import_credentials("zeta", &source, Some("uuid-zeta")).unwrap();
        import_credentials("alpha", &source, Some("uuid-alpha")).unwrap();
        fs::write(accounts_dir.join("ignore.txt"), "not an account").unwrap();
        let accounts = scan_accounts().unwrap();
        assert_eq!(accounts[0].name, "alpha");
        assert_eq!(accounts[0].uuid.as_deref(), Some("uuid-alpha"));
        assert_eq!(accounts[1].name, "zeta");
        assert_eq!(accounts[1].uuid.as_deref(), Some("uuid-zeta"));

        std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
        let _ = fs::remove_dir_all(dir);
    }
}
