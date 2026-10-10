//! Shunt-owned xAI subscription account files.
//!
//! Each account is a named file at `~/.shunt/accounts/xai/<name>.json` (or
//! `$SHUNT_XAI_ACCOUNTS_DIR/<name>.json`) in the same JSON shape as the
//! legacy single-account file (`~/.shunt/xai-auth.json`, see
//! [`crate::auth::default_xai_auth_path`]), so [`super::auth::XaiAuthStore`]
//! reads and refreshes either one. Named logins never write the legacy file,
//! and the legacy login never writes here.

use std::{io, path::PathBuf};

use crate::auth::shared;
use crate::config::AccountConfig;

// Name validation and born-private write are provider-agnostic, so they live
// in `auth::shared` and every store calls them — only the env var and subdir
// differ here.
pub use crate::auth::shared::validate_account_name;

pub fn default_accounts_dir() -> PathBuf {
    shared::default_accounts_dir("SHUNT_XAI_ACCOUNTS_DIR", "xai")
}

pub fn account_path(name: &str) -> PathBuf {
    default_accounts_dir().join(format!("{name}.json"))
}

/// Return store-managed accounts in deterministic name order. shunt reads no
/// verified upstream account identifier from an xAI login, so every scanned
/// entry gets no `uuid` and falls back (via `accounts::account_key`) to its own
/// file name as its pool identity — the Kimi and Antigravity stores' rule.
pub fn scan_accounts() -> io::Result<Vec<AccountConfig>> {
    shared::scan_account_dir(&default_accounts_dir(), |_path| None)
}

/// Store a freshly issued xAI device-flow login as a named account, in the
/// shape [`super::auth::XaiAuthStore`] reads and refreshes.
pub fn store_oauth_tokens(
    name: &str,
    access_token: &str,
    refresh_token: &str,
    id_token: Option<&str>,
) -> anyhow::Result<PathBuf> {
    validate_account_name(name)?;
    let access_token = access_token.trim();
    let refresh_token = refresh_token.trim();
    if access_token.is_empty() || access_token.chars().any(char::is_whitespace) {
        anyhow::bail!("xAI access token must be one non-empty value without whitespace");
    }
    if refresh_token.is_empty() || refresh_token.chars().any(char::is_whitespace) {
        anyhow::bail!("xAI refresh token must be one non-empty value without whitespace");
    }
    let id_token = id_token.map(str::trim).filter(|token| !token.is_empty());
    let value = super::auth::tokens_value(access_token, refresh_token, id_token);
    let path = account_path(name);
    shared::write_account_file(&path, &value)?;
    Ok(path)
}

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::xai::auth::XaiAuthStore;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use serde_json::json;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shunt-xai-store-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn jwt(exp: u64) -> String {
        let payload = json!({ "exp": exp });
        format!(
            "x.{}.y",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        )
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
    async fn oauth_tokens_round_trip_in_the_legacy_file_shape() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("oauth");
        let _env = shared::EnvVarGuard::set("SHUNT_XAI_ACCOUNTS_DIR", &dir);

        let path = store_oauth_tokens("primary", "access", "refresh", Some("id")).unwrap();
        assert_eq!(path, account_path("primary"));

        let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["tokens"]["access_token"], "access");
        assert_eq!(value["tokens"]["refresh_token"], "refresh");
        assert_eq!(value["tokens"]["id_token"], "id");
        assert!(value["last_refresh"].is_string());

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_named_account_file_is_readable_by_the_xai_auth_store() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("readable");
        let _env = shared::EnvVarGuard::set("SHUNT_XAI_ACCOUNTS_DIR", &dir);

        let access = jwt(4_000_000_000);
        let path = store_oauth_tokens("primary", &access, "refresh", None).unwrap();

        // The access token is unexpired, so `get_valid` returns it without a refresh.
        let store = XaiAuthStore::new(path, reqwest::Client::new());
        let credential = store.get_valid().await.unwrap();
        assert_eq!(credential.access_token, access);

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn oauth_tokens_reject_blank_tokens() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("blank");
        let _env = shared::EnvVarGuard::set("SHUNT_XAI_ACCOUNTS_DIR", &dir);

        assert!(store_oauth_tokens("primary", "", "refresh", None).is_err());
        assert!(store_oauth_tokens("primary", "access", "", None).is_err());
        assert!(store_oauth_tokens("primary", "has space", "refresh", None).is_err());

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn oauth_tokens_reject_invalid_names() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("names");
        let _env = shared::EnvVarGuard::set("SHUNT_XAI_ACCOUNTS_DIR", &dir);

        assert!(store_oauth_tokens("../escape", "access", "refresh", None).is_err());
        assert!(store_oauth_tokens("Primary", "access", "refresh", None).is_err());

        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn written_file_and_directory_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("perms");
        let _env = shared::EnvVarGuard::set("SHUNT_XAI_ACCOUNTS_DIR", &dir);

        let path = store_oauth_tokens("primary", "access", "refresh", None).unwrap();
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
        let _env = shared::EnvVarGuard::set("SHUNT_XAI_ACCOUNTS_DIR", &dir);

        store_oauth_tokens("zeta", "access", "refresh", None).unwrap();
        store_oauth_tokens("alpha", "access", "refresh", None).unwrap();
        fs::write(dir.join("ignore.txt"), "not an account").unwrap();
        fs::write(dir.join("Bad.json"), "{}").unwrap();

        let accounts = scan_accounts().unwrap();
        let names: Vec<&str> = accounts
            .iter()
            .map(|account| account.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);
        assert!(accounts.iter().all(|account| account.uuid.is_none()));
        assert!(accounts.iter().all(|account| account.store_entry));

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn scan_accounts_missing_dir_is_empty() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("missing");
        let _env = shared::EnvVarGuard::set("SHUNT_XAI_ACCOUNTS_DIR", &dir);

        assert!(scan_accounts().unwrap().is_empty());
    }

    #[tokio::test]
    async fn named_login_never_writes_the_legacy_file() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("legacy");
        let legacy = dir.join("legacy.json");
        let _accounts = shared::EnvVarGuard::set("SHUNT_XAI_ACCOUNTS_DIR", dir.join("accounts"));
        let _legacy = shared::EnvVarGuard::set("SHUNT_XAI_AUTH_FILE", &legacy);

        store_oauth_tokens("primary", "access", "refresh", None).unwrap();

        assert_eq!(crate::auth::default_xai_auth_path(), legacy);
        assert!(!legacy.exists());

        let _ = fs::remove_dir_all(dir);
    }
}
