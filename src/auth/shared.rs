//! Provider-agnostic credential helpers shared across the auth stores.
//!
//! These were originally defined alongside the ChatGPT/Codex store in
//! [`crate::auth::codex::auth`], but the xAI, Claude, and Cursor stores reuse
//! them (JWT expiry parsing, ISO-8601 formatting, and atomic private-file
//! writeback). They live here so no provider auth module has to reach across
//! into a sibling provider's module.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::Value;

use crate::config::AccountConfig;

const EXPIRY_BUFFER: Duration = Duration::from_secs(5 * 60);
static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

pub fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn jwt_exp(token: &str) -> Option<SystemTime> {
    let seconds = jwt_claims(token)?.get("exp")?.as_i64()?;
    if seconds < 0 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds as u64))
}

pub fn is_token_valid_at(token: &str, now: SystemTime) -> bool {
    jwt_exp(token)
        .and_then(|exp| exp.checked_sub(EXPIRY_BUFFER))
        .is_some_and(|refresh_at| now < refresh_at)
}

pub(crate) fn write_auth_file_atomic(path: &Path, value: &Value) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("auth"),
        std::process::id(),
        counter
    ));
    let bytes = serde_json::to_vec_pretty(value)?;
    // The temp file must be born private: chmod-after-write would leave a
    // window where the tokens sit at the umask default on multi-user hosts.
    if let Err(error) = write_private(&temp, &bytes).and_then(|()| fs::rename(&temp, path)) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    Ok(())
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // `mode(0o600)` only applies when the file is created, so a stale or
    // pre-created temp at this predictable path would keep its old mode.
    // Remove any leftover, then require exclusive creation: if something
    // recreates the path in between, fail instead of writing tokens into a
    // file someone else owns.
    let _ = fs::remove_file(path);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;

    let _ = fs::remove_file(path);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Validate a store account name: non-empty and `[a-z0-9-]+` only. Shared by the
/// Claude and Codex account stores so the path-safety invariant — a name can
/// never escape the accounts directory — cannot drift between them.
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

/// Resolve a provider store's accounts directory: `$<env_var>` when set, else
/// `<home>/.shunt/accounts/<subdir>` (`HOME`, falling back to `USERPROFILE` on
/// Windows where `HOME` is unset), else a working-directory-relative
/// `.shunt/accounts/<subdir>`. `env_var`/`subdir` are the only things that differ
/// between the Claude and Codex stores.
pub fn default_accounts_dir(env_var: &str, subdir: &str) -> PathBuf {
    env::var_os(env_var)
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .or_else(|| env::var_os("USERPROFILE").filter(|home| !home.is_empty()))
                .map(PathBuf::from)
                .map(|home| home.join(".shunt").join("accounts").join(subdir))
        })
        .unwrap_or_else(|| PathBuf::from(".shunt/accounts").join(subdir))
}

/// Scan a store directory for `<name>.json` account files and return name-only
/// [`AccountConfig`] entries in deterministic name order. Each entry's `uuid` is
/// produced by `uuid_for` — the Claude store reads a UUID from the file; the
/// Codex store has none and passes `|_| None`. A missing directory yields an
/// empty list (the backward-compatible "no store configured" case).
pub fn scan_account_dir(
    dir: &Path,
    uuid_for: impl Fn(&Path) -> Option<String>,
) -> io::Result<Vec<AccountConfig>> {
    let entries = match fs::read_dir(dir) {
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
            uuid: uuid_for(&path),
        });
    }
    accounts.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(accounts)
}

/// Write an account file born-private: create its parent directory `0700` on Unix
/// (no chmod-after-create window on a multi-user host), then atomically write
/// `value` via [`write_auth_file_atomic`]. Both stores import credentials this way.
pub(crate) fn write_account_file(path: &Path, value: &Value) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(parent)?;
    }
    write_auth_file_atomic(path, value)?;
    Ok(())
}

pub(crate) fn format_iso8601(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3_600;
    let minute = (day_seconds % 3_600) / 60;
    let second = day_seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let y = y + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shunt-shared-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn validate_account_name_accepts_kebab_and_rejects_the_rest() {
        for ok in ["primary", "a", "a-1", "backup-2"] {
            assert!(validate_account_name(ok).is_ok(), "rejected {ok:?}");
        }
        for bad in ["", "Bad", "a b", "under_score", "a.b", "café"] {
            assert!(validate_account_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn default_accounts_dir_prefers_the_env_override() {
        // A per-pid var name no other test reads, so no cross-test env race.
        let env_name = format!("SHUNT_TEST_SHARED_DIR_{}", std::process::id());
        std::env::set_var(&env_name, "/tmp/shunt-shared-override");
        assert_eq!(
            default_accounts_dir(&env_name, "codex"),
            PathBuf::from("/tmp/shunt-shared-override")
        );
        std::env::remove_var(&env_name);
    }

    #[test]
    fn scan_account_dir_returns_sorted_names_skipping_non_accounts() {
        let dir = temp_dir("scan");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("zeta.json"), "{}").unwrap();
        fs::write(dir.join("alpha.json"), "{}").unwrap();
        fs::write(dir.join("ignore.txt"), "x").unwrap(); // non-json extension → skipped
        fs::write(dir.join("Bad.json"), "{}").unwrap(); // invalid name → skipped
        fs::create_dir_all(dir.join("subdir.json")).unwrap(); // not a file → skipped

        let accounts = scan_account_dir(&dir, |_| None).unwrap();
        let names: Vec<_> = accounts
            .iter()
            .map(|account| account.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);
        assert!(accounts.iter().all(|account| account.uuid.is_none()));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scan_account_dir_missing_dir_is_empty() {
        let dir = temp_dir("missing").join("does-not-exist");
        assert!(scan_account_dir(&dir, |_| None).unwrap().is_empty());
    }

    #[test]
    fn write_account_file_creates_born_private_and_round_trips() {
        let dir = temp_dir("write");
        let path = dir.join("acct.json");
        let value = serde_json::json!({"k": "v"});
        write_account_file(&path, &value).unwrap();

        let read: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(read, value);
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
}
