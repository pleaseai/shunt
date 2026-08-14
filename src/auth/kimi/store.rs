//! Shunt-owned Kimi Code account files.
//!
//! Each account is a named file at `~/.shunt/accounts/kimi/<name>.json` (or
//! `$SHUNT_KIMI_ACCOUNTS_DIR/<name>.json`), read and refreshed by
//! [`super::auth::KimiAuthStore`].

use std::{io, path::PathBuf};

use serde_json::json;

use crate::auth::shared;
use crate::config::AccountConfig;

// Name validation and born-private write are provider-agnostic, so they live
// in `auth::shared` and every store calls them — only the env var and subdir
// differ here.
pub use crate::auth::shared::validate_account_name;

pub fn default_accounts_dir() -> PathBuf {
    shared::default_accounts_dir("SHUNT_KIMI_ACCOUNTS_DIR", "kimi")
}

pub fn account_path(name: &str) -> PathBuf {
    default_accounts_dir().join(format!("{name}.json"))
}

/// Return store-managed accounts in deterministic name order. Unlike the
/// Claude store (`shuntAccountUuid`) or the Codex store (`account_id`/JWT
/// claim), no Kimi Code login response has been observed to carry a stable
/// upstream account identifier — so every scanned entry gets no `uuid`,
/// falling back (via `accounts::account_identity`) to its own file name as
/// its pool identity, same as the Codex store's untagged entries.
pub fn scan_accounts() -> io::Result<Vec<AccountConfig>> {
    shared::scan_account_dir(&default_accounts_dir(), |_path| None)
}

/// Store a freshly issued Kimi Code device-flow login — access + refresh token
/// plus the account's `X-Msh-Device-Id` (generated once at login, reused on
/// every later refresh so the account presents a stable device identity to
/// Kimi) — in the shape [`super::auth::KimiAuthStore`] reads and refreshes.
pub fn store_oauth_tokens(
    name: &str,
    access_token: &str,
    refresh_token: &str,
    expires_at_ms: i64,
    device_id: &str,
) -> anyhow::Result<PathBuf> {
    validate_account_name(name)?;
    let access_token = access_token.trim();
    let refresh_token = refresh_token.trim();
    let device_id = device_id.trim();
    if access_token.is_empty() || access_token.chars().any(char::is_whitespace) {
        anyhow::bail!("Kimi access token must be one non-empty value without whitespace");
    }
    if refresh_token.is_empty() || refresh_token.chars().any(char::is_whitespace) {
        anyhow::bail!("Kimi refresh token must be one non-empty value without whitespace");
    }
    if device_id.is_empty() {
        anyhow::bail!("Kimi device id must not be empty");
    }
    let value = json!({
        "kimiOauth": {
            "accessToken": access_token,
            "refreshToken": refresh_token,
            "expiresAt": expires_at_ms
        },
        "deviceId": device_id
    });
    let path = account_path(name);
    shared::write_account_file(&path, &value)?;
    Ok(path)
}

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shunt-kimi-store-{tag}-{}-{}",
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
    async fn oauth_tokens_round_trip_with_device_id() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("oauth");
        let _env = shared::EnvVarGuard::set("SHUNT_KIMI_ACCOUNTS_DIR", &dir);

        let path = store_oauth_tokens(
            "primary",
            "access-token",
            "refresh-token",
            4_000_000_000_000,
            "device-abc-123",
        )
        .unwrap();
        assert_eq!(path, account_path("primary"));

        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["kimiOauth"]["accessToken"], "access-token");
        assert_eq!(value["kimiOauth"]["refreshToken"], "refresh-token");
        assert_eq!(value["kimiOauth"]["expiresAt"], 4_000_000_000_000_i64);
        assert_eq!(value["deviceId"], "device-abc-123");

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn oauth_tokens_reject_blank_fields() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("blank");
        let _env = shared::EnvVarGuard::set("SHUNT_KIMI_ACCOUNTS_DIR", &dir);

        assert!(store_oauth_tokens("primary", "", "refresh", 0, "device").is_err());
        assert!(store_oauth_tokens("primary", "access", "", 0, "device").is_err());
        assert!(store_oauth_tokens("primary", "access", "refresh", 0, "").is_err());

        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn written_file_and_directory_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("perms");
        let _env = shared::EnvVarGuard::set("SHUNT_KIMI_ACCOUNTS_DIR", &dir);

        let path = store_oauth_tokens("primary", "access", "refresh", 0, "device").unwrap();
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
}
