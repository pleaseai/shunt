//! Shunt-owned Codex (ChatGPT) account files.
//!
//! Each account is stored as a `codex login`-shaped file at
//! `~/.shunt/accounts/codex/<name>.json` (or
//! `$SHUNT_CODEX_ACCOUNTS_DIR/<name>.json`). Unlike the Claude store, the file
//! is copied verbatim — no `claudeAiOauth`-style wrapping and no synthetic
//! setup-token concept — so the existing [`super::auth::CodexAuthStore`]
//! reads, refreshes, and writes it exactly as it would `~/.codex/auth.json`.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};

use crate::auth::shared;
use crate::config::AccountConfig;

// The name validation, directory resolution, scan, and born-private write are
// identical to the Claude store, so they live in `auth::shared` and both stores
// call them — only the env var and subdir differ here.
pub use crate::auth::shared::validate_account_name;

pub fn default_accounts_dir() -> PathBuf {
    shared::default_accounts_dir("SHUNT_CODEX_ACCOUNTS_DIR", "codex")
}

pub fn account_path(name: &str) -> PathBuf {
    default_accounts_dir().join(format!("{name}.json"))
}

/// Return store-managed accounts in deterministic name order. Codex accounts
/// carry no UUID concept — the account id lives inside the token, not the
/// account entry — so every entry is name-only (`uuid: None`).
pub fn scan_accounts() -> io::Result<Vec<AccountConfig>> {
    shared::scan_account_dir(&default_accounts_dir(), |_| None)
}

/// Token-free Codex account metadata exposed by the admin dashboard.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CodexAccountMeta {
    pub name: String,
    /// Access-token JWT expiry in Unix epoch milliseconds, when parseable.
    pub expires_at: Option<i64>,
    pub account_id: Option<String>,
}

/// Read one store account's token-free metadata. `None` when the file is missing
/// or cannot be parsed; failures are logged without exposing token material.
pub fn account_meta(name: &str) -> Option<CodexAccountMeta> {
    let path = account_path(name);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(account = %name, %error, "admin: failed to read Codex account file; omitting from dashboard");
            return None;
        }
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(account = %name, %error, "admin: Codex account file is not valid JSON; omitting from dashboard");
            return None;
        }
    };
    let Some(tokens) = value.get("tokens") else {
        tracing::warn!(account = %name, "admin: Codex account file has no tokens object; omitting from dashboard");
        return None;
    };
    let access_token = tokens.get("access_token").and_then(Value::as_str);
    let expires_at = access_token
        .and_then(shared::jwt_exp)
        .and_then(|expiry| expiry.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_millis()).ok());
    let account_id = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .filter(|account_id| !account_id.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| access_token.and_then(super::auth::jwt_account_id));
    Some(CodexAccountMeta {
        name: name.to_string(),
        expires_at,
        account_id,
    })
}

/// List store-managed Codex accounts with token-free metadata in name order.
pub fn list_account_meta() -> io::Result<Vec<CodexAccountMeta>> {
    Ok(scan_accounts()?
        .into_iter()
        .filter_map(|account| account_meta(&account.name))
        .collect())
}

/// Remove a store account file. Returns whether a file was actually removed
/// (`false` when it did not exist). The name is validated so a caller-supplied
/// value can never escape the accounts directory. This deletes an
/// operator-owned import file only; it never touches upstream ChatGPT state.
pub fn remove_account(name: &str) -> anyhow::Result<bool> {
    validate_account_name(name)?;
    match fs::remove_file(account_path(name)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Import a refreshable Codex (ChatGPT) credential file without changing the
/// source. Unlike Claude's `import_credentials`, the on-disk shape is copied
/// verbatim: `CodexAuthStore` expects the same
/// `{auth_mode, tokens: {access_token, refresh_token, ...}, last_refresh}`
/// shape that `codex login` itself writes, so there is no wrapping to apply.
pub fn import_auth(name: &str, source: &Path) -> anyhow::Result<PathBuf> {
    validate_account_name(name)?;
    let value: Value = serde_json::from_slice(&fs::read(source)?).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Codex credentials JSON: {error}"),
        )
    })?;
    if value.get("auth_mode").and_then(Value::as_str) != Some("ChatGPT") {
        anyhow::bail!(
            "{} is not a ChatGPT login (auth_mode != \"ChatGPT\"); run `codex login` first",
            source.display()
        );
    }
    let tokens = value.get("tokens");
    let has_access = tokens
        .and_then(|tokens| tokens.get("access_token"))
        .and_then(Value::as_str)
        .is_some_and(|token| !token.is_empty());
    let has_refresh = tokens
        .and_then(|tokens| tokens.get("refresh_token"))
        .and_then(Value::as_str)
        .is_some_and(|token| !token.is_empty());
    if !has_access || !has_refresh {
        anyhow::bail!(
            "{} does not contain refreshable Codex tokens",
            source.display()
        );
    }
    write_account(name, &value)
}

/// Store a freshly issued ChatGPT OAuth credential in the verbatim `codex login`
/// auth.json shape consumed and refreshed by [`super::auth::CodexAuthStore`].
pub fn store_chatgpt_tokens(
    name: &str,
    access_token: &str,
    refresh_token: &str,
    id_token: Option<&str>,
    account_id: &str,
) -> anyhow::Result<PathBuf> {
    validate_account_name(name)?;
    let account_id = account_id.trim();
    let refresh_token = refresh_token.trim();
    if account_id.is_empty() {
        anyhow::bail!("ChatGPT account id must not be empty");
    }
    if refresh_token.is_empty() {
        anyhow::bail!("ChatGPT refresh token must not be empty");
    }
    let mut tokens = json!({
        "access_token": access_token,
        "refresh_token": refresh_token,
        "account_id": account_id,
    });
    if let Some(id_token) = id_token {
        tokens
            .as_object_mut()
            .expect("ChatGPT tokens are a JSON object")
            .insert("id_token".to_string(), json!(id_token));
    }
    let value = json!({
        "auth_mode": "ChatGPT",
        "OPENAI_API_KEY": null,
        "tokens": tokens,
        "last_refresh": shared::format_iso8601(SystemTime::now()),
    });
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
            "shunt-codex-store-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn chatgpt_source_json(access_token: &str, refresh_token: &str) -> String {
        serde_json::json!({
            "auth_mode": "ChatGPT",
            "OPENAI_API_KEY": null,
            "tokens": {
                "access_token": access_token,
                "refresh_token": refresh_token,
                "id_token": "id",
                "account_id": "acct"
            },
            "last_refresh": "2024-01-01T00:00:00Z"
        })
        .to_string()
    }

    fn access_token(exp: u64, account_id: &str) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

        let payload = json!({
            "exp": exp,
            "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
        });
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

    #[test]
    fn import_rejects_non_chatgpt_auth_mode() {
        let dir = temp_dir("api-key-mode");
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.json");
        fs::write(
            &source,
            r#"{"auth_mode":"ApiKey","OPENAI_API_KEY":"key","tokens":null}"#,
        )
        .unwrap();

        let error = import_auth("ci", &source).unwrap_err();
        assert!(error.to_string().contains("auth_mode"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn import_rejects_missing_refresh_token() {
        let dir = temp_dir("no-refresh");
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.json");
        fs::write(
            &source,
            r#"{"auth_mode":"ChatGPT","tokens":{"access_token":"access"}}"#,
        )
        .unwrap();

        assert!(import_auth("ci", &source).is_err());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn import_rejects_unparseable_json() {
        let dir = temp_dir("bad-json");
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.json");
        fs::write(&source, "not valid json {").unwrap();

        let error = import_auth("ci", &source).unwrap_err();
        assert!(
            error.to_string().contains("invalid Codex credentials JSON"),
            "got: {error}"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn imports_and_scans_refreshable_accounts_in_name_order() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("import");
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.json");
        fs::write(&source, chatgpt_source_json("access", "refresh")).unwrap();
        let accounts_dir = dir.join("accounts");
        // Declared after TEST_ENV_LOCK so it drops first: the var is removed on
        // drop (panic-safe) while the lock is still held.
        let _env = shared::EnvVarGuard::set("SHUNT_CODEX_ACCOUNTS_DIR", &accounts_dir);

        let path = import_auth("zeta", &source).unwrap();
        import_auth("alpha", &source).unwrap();
        fs::write(accounts_dir.join("ignore.txt"), "not an account").unwrap();
        let accounts = scan_accounts().unwrap();
        assert_eq!(accounts[0].name, "alpha");
        assert_eq!(accounts[0].uuid, None);
        assert_eq!(accounts[1].name, "zeta");

        // The imported file preserves the raw codex auth.json shape verbatim —
        // no claudeAiOauth-style wrapping.
        let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["auth_mode"], "ChatGPT");
        assert_eq!(saved["tokens"]["access_token"], "access");
        assert_eq!(saved["tokens"]["refresh_token"], "refresh");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&accounts_dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn stores_tokens_and_lists_token_free_metadata() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("oauth");
        let _env = shared::EnvVarGuard::set("SHUNT_CODEX_ACCOUNTS_DIR", &dir);
        let access = access_token(2_000_000_000, "acct-from-claim");

        let path = store_chatgpt_tokens(
            "oauth",
            &access,
            "refresh-secret",
            Some("id-secret"),
            "acct-stored",
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["auth_mode"], "ChatGPT");
        assert!(value["OPENAI_API_KEY"].is_null());
        assert_eq!(value["tokens"]["access_token"], access);
        assert_eq!(value["tokens"]["refresh_token"], "refresh-secret");
        assert_eq!(value["tokens"]["id_token"], "id-secret");
        assert_eq!(value["tokens"]["account_id"], "acct-stored");
        assert!(value["last_refresh"].as_str().is_some());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        let meta = list_account_meta().unwrap();
        assert_eq!(meta.len(), 1);
        assert_eq!(meta[0].name, "oauth");
        assert_eq!(meta[0].expires_at, Some(2_000_000_000_000));
        assert_eq!(meta[0].account_id.as_deref(), Some("acct-stored"));
        let serialized = serde_json::to_string(&meta).unwrap();
        assert!(!serialized.contains(&access));
        assert!(!serialized.contains("refresh-secret"));
        assert!(!serialized.contains("id-secret"));

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn metadata_falls_back_to_access_token_account_claim() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("meta-claim");
        fs::create_dir_all(&dir).unwrap();
        let _env = shared::EnvVarGuard::set("SHUNT_CODEX_ACCOUNTS_DIR", &dir);
        let access = access_token(2_000_000_000, "acct-from-claim");
        fs::write(
            dir.join("claim.json"),
            json!({"auth_mode":"ChatGPT","tokens":{"access_token":access}}).to_string(),
        )
        .unwrap();

        let meta = account_meta("claim").unwrap();
        assert_eq!(meta.account_id.as_deref(), Some("acct-from-claim"));
        assert_eq!(meta.expires_at, Some(2_000_000_000_000));

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn removes_existing_and_reports_missing_accounts() {
        let _guard = TEST_ENV_LOCK.lock().await;
        let dir = temp_dir("remove");
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.json");
        fs::write(&source, chatgpt_source_json("access", "refresh")).unwrap();
        let accounts_dir = dir.join("accounts");
        let _env = shared::EnvVarGuard::set("SHUNT_CODEX_ACCOUNTS_DIR", &accounts_dir);

        import_auth("ci", &source).unwrap();
        assert!(remove_account("ci").unwrap());
        assert!(!remove_account("ci").unwrap());

        let _ = fs::remove_dir_all(dir);
    }
}
