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

use crate::auth::codex_auth::write_auth_file_atomic;
use crate::config::AccountConfig;

const SETUP_TOKEN_LIFETIME: Duration = Duration::from_secs(365 * 24 * 60 * 60);

pub fn default_accounts_dir() -> PathBuf {
    env::var_os("SHUNT_CLAUDE_ACCOUNTS_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
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
        let entry = entry?;
        if !entry.file_type()?.is_file() {
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
pub fn import_credentials(name: &str, source: &Path) -> anyhow::Result<PathBuf> {
    validate_account_name(name)?;
    let value: Value = serde_json::from_slice(&fs::read(source)?).map_err(|error| {
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
    write_account(name, &value)
}

/// Store a one-year token in the shape consumed by `ClaudeAuthStore`.
pub fn store_setup_token(name: &str, token: &str) -> anyhow::Result<PathBuf> {
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
    write_account(
        name,
        &json!({
            "claudeAiOauth": {
                "accessToken": token,
                "expiresAt": expires_at,
                "shuntCredentialKind": "setup_token"
            }
        }),
    )
}

fn write_account(name: &str, value: &Value) -> anyhow::Result<PathBuf> {
    let path = account_path(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        set_private_directory_permissions(parent)?;
    }
    write_auth_file_atomic(&path, value)?;
    Ok(path)
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
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

        let path = store_setup_token("ci", "token-one").unwrap();
        store_setup_token("ci", "token-two").unwrap();
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["claudeAiOauth"]["accessToken"], "token-two");
        assert_eq!(value["claudeAiOauth"]["shuntCredentialKind"], "setup_token");
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

        import_credentials("zeta", &source).unwrap();
        import_credentials("alpha", &source).unwrap();
        fs::write(accounts_dir.join("ignore.txt"), "not an account").unwrap();
        let names = scan_accounts()
            .unwrap()
            .into_iter()
            .map(|account| account.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["alpha", "zeta"]);

        std::env::remove_var("SHUNT_CLAUDE_ACCOUNTS_DIR");
        let _ = fs::remove_dir_all(dir);
    }
}
