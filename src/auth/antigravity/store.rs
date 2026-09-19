//! Shunt-owned Antigravity subscription account files.
//!
//! Each account is a named file at `~/.shunt/accounts/antigravity/<name>.json`
//! (or `$SHUNT_ANTIGRAVITY_ACCOUNTS_DIR/<name>.json`), a sibling of the other
//! provider stores under `auth::shared`. The singleton credential file
//! (`default_antigravity_auth_path`) is untouched: named logins never overwrite
//! it, and the singleton login never writes here.

use std::io;
use std::path::PathBuf;

use serde_json::json;

use crate::auth::shared;
use crate::config::AccountConfig;

// Name validation and born-private write are provider-agnostic, so they live
// in `auth::shared` and every store calls them — only the env var and subdir
// differ here.
pub use crate::auth::shared::validate_account_name;

pub fn default_accounts_dir() -> PathBuf {
    shared::default_accounts_dir("SHUNT_ANTIGRAVITY_ACCOUNTS_DIR", "antigravity")
}

pub fn account_path(name: &str) -> PathBuf {
    default_accounts_dir().join(format!("{name}.json"))
}

/// Return store-managed accounts in deterministic name order. Unlike the
/// Claude store (`shuntAccountUuid`) or the Codex store (`account_id`/JWT
/// claim), no Antigravity login response has been observed to carry a stable
/// upstream account identifier shunt can read — the userinfo email is the only
/// candidate, and it is optional, unverified as a stable identity, and never
/// a substitute for the file name — so every scanned entry gets no `uuid`,
/// falling back (via `accounts::account_identity`) to its own file name as
/// its pool identity, same as the Codex store's untagged entries.
pub fn scan_accounts() -> io::Result<Vec<AccountConfig>> {
    shared::scan_account_dir(&default_accounts_dir(), |_path| None)
}

/// Store a freshly issued Antigravity OAuth login — access + refresh token,
/// optional label email, and the Code Assist project id — in the flat
/// [`super::auth::StoredAuth`] schema the singleton file uses, so
/// [`super::auth::AntigravityAuthStore`] reads a named account file exactly
/// like the singleton one.
/// The store account's runtime identity, or `None` when no account file
/// exists for `name` at all. Antigravity accounts never carry a `uuid` (see
/// [`scan_accounts`]), so — unlike the Claude/Codex stores — the identity is
/// always the account's own name; this only distinguishes "present" from
/// "no such account" for reprovision/removal cleanup.
pub fn account_identity(name: &str) -> Option<String> {
    account_path(name).exists().then(|| name.to_string())
}

fn read_stored(path: &std::path::Path) -> Result<super::auth::StoredAuth, ()> {
    let bytes = std::fs::read(path).map_err(|_| ())?;
    serde_json::from_slice(&bytes).map_err(|_| ())
}

/// Like [`scan_accounts`], but a per-file read/parse failure aborts the whole
/// scan (`Err`) instead of silently treating that account as identity-less.
/// Used by admin cleanup's fail-closed check — see
/// [`shared::scan_account_dir_strict`].
pub fn scan_accounts_strict() -> io::Result<Vec<AccountConfig>> {
    shared::scan_account_dir_strict(&default_accounts_dir(), |path| {
        read_stored(path).map(|_| None)
    })
}

/// Token-free Antigravity account metadata exposed by the admin dashboard.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AntigravityAccountMeta {
    pub name: String,
    /// Stored `expiry_date`, already Unix epoch milliseconds — no JWT to parse.
    pub expires_at: Option<i64>,
    pub email: Option<String>,
    pub project_id: Option<String>,
}

/// Read one store account's token-free metadata. `None` when the file is
/// missing or cannot be parsed; failures are logged without exposing token
/// material.
pub fn account_meta(name: &str) -> Option<AntigravityAccountMeta> {
    let path = account_path(name);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(account = %name, %error, "admin: failed to read Antigravity account file; omitting from dashboard");
            return None;
        }
    };
    let stored: super::auth::StoredAuth = match serde_json::from_slice(&bytes) {
        Ok(stored) => stored,
        Err(error) => {
            tracing::warn!(account = %name, %error, "admin: Antigravity account file is not valid JSON; omitting from dashboard");
            return None;
        }
    };
    Some(AntigravityAccountMeta {
        name: name.to_string(),
        expires_at: stored
            .expiry_date
            .and_then(|millis| i64::try_from(millis).ok()),
        email: stored.email,
        project_id: stored.project_id,
    })
}

/// List store-managed Antigravity accounts with token-free metadata in name order.
pub fn list_account_meta() -> io::Result<Vec<AntigravityAccountMeta>> {
    Ok(scan_accounts()?
        .into_iter()
        .filter_map(|account| account_meta(&account.name))
        .collect())
}

/// Remove a store account file. Returns whether a file was actually removed
/// (`false` when it did not exist). The name is validated so a caller-supplied
/// value can never escape the accounts directory. This deletes an
/// operator-owned import file only; it never touches upstream Antigravity state.
pub fn remove_account(name: &str) -> anyhow::Result<bool> {
    validate_account_name(name)?;
    match std::fs::remove_file(account_path(name)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// The stored access token for a named account, or `None` when missing/unreadable.
/// Admin-only: feeds [`super::auth::AntigravityAuthStore::force_refresh_if_access_token`]
/// so the refresh probe can tell whether the on-disk token is still the one
/// being refreshed, without exposing it to the dashboard response.
pub(crate) fn stored_access_token(name: &str) -> Option<String> {
    read_stored(&account_path(name))
        .ok()
        .map(|stored| stored.access_token)
}

pub fn store_oauth_tokens(
    name: &str,
    access_token: &str,
    refresh_token: &str,
    expiry_date: Option<u64>,
    email: Option<&str>,
    project_id: Option<&str>,
) -> anyhow::Result<PathBuf> {
    validate_account_name(name)?;
    let access_token = access_token.trim();
    if access_token.is_empty() || access_token.chars().any(char::is_whitespace) {
        anyhow::bail!("Antigravity access token must be one non-empty value without whitespace");
    }
    let refresh_token = refresh_token.trim();
    if refresh_token.is_empty() || refresh_token.chars().any(char::is_whitespace) {
        anyhow::bail!("Antigravity refresh token must be one non-empty value without whitespace");
    }
    let email = email.map(str::trim).filter(|email| !email.is_empty());
    let project_id = project_id.map(str::trim).filter(|id| !id.is_empty());
    let value = json!({
        "access_token": access_token,
        "refresh_token": refresh_token,
        "expiry_date": expiry_date,
        "email": email,
        "project_id": project_id,
    });
    let path = account_path(name);
    super::auth::write_named_account(&path, &value)?;
    Ok(path)
}

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shunt-antigravity-store-{tag}-{}-{}",
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
    async fn oauth_tokens_round_trip_in_the_stored_auth_schema() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("oauth");
        let _env = shared::EnvVarGuard::set("SHUNT_ANTIGRAVITY_ACCOUNTS_DIR", &dir);

        let path = store_oauth_tokens(
            "primary",
            "access-token",
            "refresh-token",
            Some(4_000_000_000_000),
            Some("a@example.com"),
            Some("proj-1"),
        )
        .unwrap();
        assert_eq!(path, account_path("primary"));

        // The named file must deserialize as the exact `StoredAuth` record the
        // singleton store reads — that is the whole point of the flat schema.
        let written: super::super::auth::StoredAuth =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(written.access_token, "access-token");
        assert_eq!(written.refresh_token, "refresh-token");
        assert_eq!(written.expiry_date, Some(4_000_000_000_000));
        assert_eq!(written.email.as_deref(), Some("a@example.com"));
        assert_eq!(written.project_id.as_deref(), Some("proj-1"));

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn oauth_tokens_reject_blank_tokens_and_trim_labels() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("blank");
        let _env = shared::EnvVarGuard::set("SHUNT_ANTIGRAVITY_ACCOUNTS_DIR", &dir);

        assert!(store_oauth_tokens("primary", "", "refresh", None, None, None).is_err());
        assert!(store_oauth_tokens("primary", " ", "refresh", None, None, None).is_err());
        assert!(store_oauth_tokens("primary", "access", "", None, None, None).is_err());
        assert!(store_oauth_tokens("primary", "access", " ", None, None, None).is_err());

        // Email and project id are labels, not secrets: blank ones degrade to
        // None rather than failing the login.
        let path = store_oauth_tokens(
            "primary",
            "access",
            "refresh",
            None,
            Some("   "),
            Some("  "),
        )
        .unwrap();
        let written: super::super::auth::StoredAuth =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(written.email, None);
        assert_eq!(written.project_id, None);

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn oauth_tokens_reject_invalid_names() {
        assert!(store_oauth_tokens("../escape", "access", "refresh", None, None, None).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn written_file_and_directory_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("perms");
        let _env = shared::EnvVarGuard::set("SHUNT_ANTIGRAVITY_ACCOUNTS_DIR", &dir);

        let path = store_oauth_tokens("primary", "access", "refresh", None, None, None).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn scan_accounts_skips_non_account_files_and_uses_filename_identity() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("scan");
        let _env = shared::EnvVarGuard::set("SHUNT_ANTIGRAVITY_ACCOUNTS_DIR", &dir);
        fs::create_dir_all(&dir).unwrap();

        store_oauth_tokens("zeta", "a", "r", None, None, None).unwrap();
        store_oauth_tokens("alpha", "a", "r", None, None, None).unwrap();
        fs::write(dir.join("ignore.txt"), "x").unwrap();
        fs::write(dir.join("Bad.json"), "{}").unwrap();

        let accounts = scan_accounts().unwrap();
        let names: Vec<_> = accounts
            .iter()
            .map(|account| account.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);
        // Filename identity: no uuid, so `accounts::account_identity` falls
        // back to the file name — never an unverified email.
        assert!(accounts.iter().all(|account| account.uuid.is_none()));

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn scan_accounts_missing_dir_is_empty() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("missing").join("does-not-exist");
        let _env = shared::EnvVarGuard::set("SHUNT_ANTIGRAVITY_ACCOUNTS_DIR", &dir);
        assert!(scan_accounts().unwrap().is_empty());
    }
}
