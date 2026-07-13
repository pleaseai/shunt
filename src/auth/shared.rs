//! Provider-agnostic credential helpers shared across the auth stores.
//!
//! These were originally defined alongside the ChatGPT/Codex store in
//! [`crate::auth::codex_auth`], but the xAI, Claude, and Cursor stores reuse
//! them (JWT expiry parsing, ISO-8601 formatting, and atomic private-file
//! writeback). They live here so no provider auth module has to reach across
//! into a sibling provider's module.

use std::{
    fs, io,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::Value;

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
