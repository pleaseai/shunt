//! Shunt-owned ChatGPT-plan account files and the installation host id.
//!
//! Each account is a named file at `~/.shunt/accounts/chatgpt-plan/<name>.json`
//! (or `$SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR/<name>.json`). The installation's host
//! id lives beside them in `.host-id`; the account scan only reads `*.json`
//! files with valid names, so neither it nor a `<name>.json.lock` sibling is
//! ever mistaken for an account.
//!
//! Writes are born-private (directory 0700, file 0600, atomic) and take the
//! cross-process lock, because the refresh token rotates and the CLI can run
//! beside the gateway. Reads take no lock: the atomic rename means a reader
//! sees a whole old or whole new file.

use std::{fs, io, path::PathBuf, time::Duration};

use anyhow::Context;

use super::record::{AccountRecord, RECORD_VERSION};
use crate::auth::shared::{self, file_lock::FileLockKind};
use crate::config::AccountConfig;

pub use crate::auth::shared::validate_account_name;

/// Cross-process exclusion over chatgpt-plan account and host-id files.
pub static ACCOUNT_FILE_LOCK: FileLockKind = FileLockKind {
    lock_name: "ChatGPT plan account lock",
    contention_hint: "Another shunt process is writing a chatgpt-plan account (`shunt login \
                      chatgpt-plan`, `shunt logout`, or a token refresh). The lock releases when \
                      that process exits, so wait for it and retry. Do not delete the lock file: \
                      removing it while a writer holds it lets the next writer lock a new inode \
                      and serialize against nothing",
    task_context: "ChatGPT plan account lock task failed",
    unsupported_warning: "Warning: this platform has no advisory file lock, so a `shunt login \
                          chatgpt-plan` running alongside the gateway is not serialized against \
                          its token writeback. A rotated refresh token can be lost that way; \
                          sign in again if that happens.",
    #[cfg(not(unix))]
    warned: std::sync::Once::new(),
};

/// How long a writer waits for [`ACCOUNT_FILE_LOCK`]. A login or logout holds
/// it for one read plus one atomic write of a small file; a token refresh
/// holds it across its read, the token-endpoint POST (bounded by the refresh
/// timeout), and its write, because the refresh token rotates.
pub const ACCOUNT_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

const HOST_ID_FILE: &str = ".host-id";
const HOST_ID_PREFIX: &str = "urn:uuid:";

pub fn default_accounts_dir() -> PathBuf {
    shared::default_accounts_dir("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", "chatgpt-plan")
}

pub fn account_path(name: &str) -> PathBuf {
    default_accounts_dir().join(format!("{name}.json"))
}

/// Return store-managed accounts in deterministic name order. No verified
/// upstream account identifier is read, so every entry falls back to its own
/// file name as its pool identity: two names with the same email stay two
/// accounts.
pub fn scan_accounts() -> io::Result<Vec<AccountConfig>> {
    shared::scan_account_dir(&default_accounts_dir(), |_path| None)
}

/// Read one account. `Ok(None)` means no such file; a file that does not parse,
/// or carries another `version`, is an error.
pub fn read_account(name: &str) -> anyhow::Result<Option<AccountRecord>> {
    validate_account_name(name)?;
    let path = account_path(name);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("failed to read {name}")),
    };
    let record: AccountRecord = serde_json::from_slice(&bytes)
        .with_context(|| format!("chatgpt-plan account {name} is not a valid account file"))?;
    anyhow::ensure!(
        record.version == RECORD_VERSION,
        "chatgpt-plan account {name} has unsupported version {}",
        record.version
    );
    Ok(Some(record))
}

/// Write `record` as account `name`, replacing any existing file.
pub fn write_account(name: &str, record: &AccountRecord) -> anyhow::Result<()> {
    validate_account_name(name)?;
    let value = serde_json::to_value(record)?;
    shared::write_account_file_locked(
        &account_path(name),
        &value,
        &ACCOUNT_FILE_LOCK,
        ACCOUNT_LOCK_TIMEOUT,
    )
}

/// Write `record` as account `name` while the caller already holds
/// [`ACCOUNT_FILE_LOCK`] for it (`_lock`): the one lock-held write path, for a
/// read-modify-write that must stay atomic across processes. The guard must be
/// the lock of this account's file, taken with [`account_path`].
pub fn write_account_locked(
    _lock: &shared::file_lock::FileLock,
    name: &str,
    record: &AccountRecord,
) -> anyhow::Result<()> {
    validate_account_name(name)?;
    shared::write_auth_file_atomic(&account_path(name), &serde_json::to_value(record)?)?;
    Ok(())
}

/// Write account `name` signed out: only `client_id` (and `version`) remain.
pub fn write_signed_out(name: &str, client_id: &str) -> anyhow::Result<()> {
    write_account(name, &AccountRecord::signed_out(client_id))
}

/// The installation host id, created exactly once. Concurrent first callers
/// (threads or processes) serialize on the file lock and all return the id the
/// first one wrote.
pub fn host_id() -> anyhow::Result<String> {
    let path = default_accounts_dir().join(HOST_ID_FILE);
    if let Some(id) = read_host_id(&path)? {
        return Ok(id);
    }
    shared::create_private_dir(path.parent().expect("host id path has a parent"))?;
    let _guard =
        shared::file_lock::lock_file_blocking(&path, &ACCOUNT_FILE_LOCK, ACCOUNT_LOCK_TIMEOUT)?;
    // Re-read under the lock: another caller may have won the race.
    if let Some(id) = read_host_id(&path)? {
        return Ok(id);
    }
    let id = format!("{HOST_ID_PREFIX}{}", uuid::Uuid::new_v4());
    crate::atomic_file::write_private_atomic_in_existing_dir(&path, id.as_bytes())
        .context("failed to write the installation host id")?;
    Ok(id)
}

fn read_host_id(path: &std::path::Path) -> anyhow::Result<Option<String>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("failed to read the installation host id"),
    };
    let text = text.trim();
    // A blank or malformed file is replaced rather than handed out as an id.
    let valid = text
        .strip_prefix(HOST_ID_PREFIX)
        .is_some_and(|uuid| uuid::Uuid::parse_str(uuid).is_ok());
    Ok(valid.then(|| text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shunt-chatgpt-plan-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn signed_in(client_id: &str, email: &str) -> AccountRecord {
        AccountRecord {
            version: RECORD_VERSION,
            client_id: client_id.into(),
            sub: Some("user-1".into()),
            email: Some(email.into()),
            scopes: vec!["openid".into(), "offline_access".into()],
            tokens: Some(super::super::record::TokenBlock {
                access_token: "access-secret".into(),
                refresh_token: "refresh-secret".into(),
                id_token: Some("id-secret".into()),
                expires_at: 4_000_000_000,
                earliest_refresh_at: Some(3_999_999_000),
                last_refresh: Some(1_000),
            }),
        }
    }

    fn names(accounts: Vec<AccountConfig>) -> Vec<String> {
        accounts.into_iter().map(|account| account.name).collect()
    }

    #[test]
    fn signed_in_account_round_trips_and_is_private() {
        let _lock = crate::config::test_env_lock();
        let dir = temp_dir("rw");
        let _env = shared::EnvVarGuard::set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &dir);

        let record = signed_in("oaiapp_one", "a@example.com");
        write_account("primary", &record).unwrap();
        assert_eq!(read_account("primary").unwrap(), Some(record));
        assert_eq!(read_account("missing").unwrap(), None);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&account_path("primary")), 0o600);
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn signed_out_account_is_exactly_version_and_client_id() {
        let _lock = crate::config::test_env_lock();
        let dir = temp_dir("out");
        let _env = shared::EnvVarGuard::set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &dir);

        write_account("primary", &signed_in("oaiapp_one", "a@example.com")).unwrap();
        let kept = read_account("primary").unwrap().unwrap().client_id;
        write_signed_out("primary", &kept).unwrap();

        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(account_path("primary")).unwrap()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"version": 1, "client_id": "oaiapp_one"})
        );
        let record = read_account("primary").unwrap().unwrap();
        assert_eq!(record, AccountRecord::signed_out("oaiapp_one"));
        assert!(!record.is_signed_in());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn unsupported_version_and_bad_names_are_rejected() {
        let _lock = crate::config::test_env_lock();
        let dir = temp_dir("bad");
        let _env = shared::EnvVarGuard::set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &dir);

        fs::create_dir_all(&dir).unwrap();
        fs::write(account_path("old"), r#"{"version":2,"client_id":"x"}"#).unwrap();
        assert!(read_account("old").is_err());
        assert!(read_account("../escape").is_err());
        assert!(write_signed_out("Bad Name", "x").is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn same_email_under_two_names_is_two_accounts() {
        let _lock = crate::config::test_env_lock();
        let dir = temp_dir("pool");
        let _env = shared::EnvVarGuard::set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &dir);

        write_account("work", &signed_in("oaiapp_a", "same@example.com")).unwrap();
        write_account("home", &signed_in("oaiapp_b", "same@example.com")).unwrap();
        let accounts = scan_accounts().unwrap();
        assert_eq!(accounts.len(), 2);
        assert!(accounts
            .iter()
            .all(|account| account.store_entry && account.uuid.is_none()));
        assert_eq!(names(accounts), ["home", "work"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn host_id_and_lock_files_are_never_scanned_as_accounts() {
        let _lock = crate::config::test_env_lock();
        let dir = temp_dir("scan");
        let _env = shared::EnvVarGuard::set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &dir);

        host_id().unwrap();
        write_signed_out("primary", "oaiapp_one").unwrap();
        assert!(dir.join(".host-id").is_file());
        assert!(dir.join("primary.json.lock").is_file());
        assert_eq!(names(scan_accounts().unwrap()), ["primary"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn host_id_is_a_urn_uuid_v4_created_once_under_concurrent_first_use() {
        let _lock = crate::config::test_env_lock();
        let dir = temp_dir("host");
        let _env = shared::EnvVarGuard::set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &dir);

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    host_id().unwrap()
                })
            })
            .collect();
        let ids: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        assert!(ids.iter().all(|id| *id == ids[0]), "ids diverged: {ids:?}");
        let uuid = ids[0].strip_prefix("urn:uuid:").expect("urn:uuid: prefix");
        assert_eq!(uuid::Uuid::parse_str(uuid).unwrap().get_version_num(), 4);
        assert_eq!(fs::read_to_string(dir.join(".host-id")).unwrap(), ids[0]);
        assert_eq!(host_id().unwrap(), ids[0]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_corrupt_host_id_file_is_replaced_with_a_valid_one() {
        let _lock = crate::config::test_env_lock();
        let dir = temp_dir("corrupt");
        let _env = shared::EnvVarGuard::set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &dir);

        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".host-id"), "garbage").unwrap();
        let id = host_id().unwrap();
        assert!(id.starts_with("urn:uuid:") && uuid::Uuid::parse_str(&id[9..]).is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn spaces_and_symlinked_store_dirs_keep_host_id_and_accounts_together() {
        let _lock = crate::config::test_env_lock();
        let root = temp_dir("link");
        let real = root.join("real store");
        fs::create_dir_all(&real).unwrap();
        let link = root.join("link to store");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let _env = shared::EnvVarGuard::set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &link);
        let id = host_id().unwrap();
        write_signed_out("primary", "oaiapp_one").unwrap();
        assert_eq!(fs::read_to_string(real.join(".host-id")).unwrap(), id);
        assert!(real.join("primary.json").is_file());
        assert_eq!(names(scan_accounts().unwrap()), ["primary"]);
        drop(_env);

        // The same directory reached by its real path (with spaces) agrees.
        let _env = shared::EnvVarGuard::set("SHUNT_CHATGPT_PLAN_ACCOUNTS_DIR", &real);
        assert_eq!(host_id().unwrap(), id);
        assert!(read_account("primary").unwrap().is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn debug_output_never_prints_token_values() {
        let rendered = format!("{:?}", signed_in("oaiapp_one", "a@example.com"));
        for secret in ["access-secret", "refresh-secret", "id-secret"] {
            assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
        }
        assert!(rendered.contains("oaiapp_one"));
    }
}
