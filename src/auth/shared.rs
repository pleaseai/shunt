//! Provider-agnostic credential helpers shared across the auth stores.
//!
//! These were originally defined alongside the ChatGPT/Codex store in
//! [`crate::auth::codex::auth`], but the xAI, Claude, and Cursor stores reuse
//! them (JWT expiry parsing, ISO-8601 formatting, and atomic private-file
//! writeback). They live here so no provider auth module has to reach across
//! into a sibling provider's module.

use std::{
    collections::HashMap,
    env, fs, io,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{accounts::StoreFamily, config::AccountConfig};

const EXPIRY_BUFFER: Duration = Duration::from_secs(5 * 60);

/// A freshly generated PKCE verifier/challenge plus an independent OAuth state.
pub(crate) struct PkceChallenge {
    pub verifier: String,
    pub challenge: String,
    pub state: String,
}

/// Generate a fresh PKCE verifier + S256 challenge and an independent `state`,
/// each 32 random bytes URL-safe base64 (no padding).
pub(crate) fn generate_pkce() -> PkceChallenge {
    let verifier = random_urlsafe(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_urlsafe(32);
    PkceChallenge {
        verifier,
        challenge,
        state,
    }
}

fn random_urlsafe(bytes: usize) -> String {
    let mut random = vec![0_u8; bytes];
    rand::rng().fill_bytes(&mut random);
    URL_SAFE_NO_PAD.encode(random)
}

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

/// Whether the long-lived `refresh_token` may be POSTed to `url`: HTTPS anywhere,
/// or plain `http://` only to loopback. Vets the initial URL and each redirect hop.
fn is_safe_refresh_url(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        || (url.scheme() == "http"
            && crate::config::host_is_loopback(url.host_str().unwrap_or_default()))
}

/// Resolve an OAuth refresh endpoint from a `SHUNT_*_TOKEN_URL` override; an empty,
/// malformed, or unsafe one (see [`is_safe_refresh_url`]) falls back to `default_url`.
pub(crate) fn sanitize_token_url(raw: Option<String>, default_url: &str) -> String {
    raw.filter(|value| !value.is_empty())
        .filter(|value| {
            value
                .parse::<reqwest::Url>()
                .is_ok_and(|url| is_safe_refresh_url(&url))
        })
        .unwrap_or_else(|| default_url.to_string())
}

/// The admin-web counterpart to [`sanitize_token_url`]: resolve a `SHUNT_*_TOKEN_URL`
/// override for the browser completion flow, but **warn** on an invalid or unsafe
/// override instead of falling back silently. The completion handler consumes the
/// single-use OAuth authorization code, so an operator who typos a local-testing
/// override would otherwise burn their real code against the production endpoint with
/// no trace in the logs. `env_var` names the override so one message serves every
/// provider; the raw value is never logged (it may embed credentials in userinfo —
/// `https://user:pass@host`), only its scheme/host, which is all the guard turns on.
pub(crate) fn admin_token_url_override(env_var: &str, default_url: &str) -> String {
    let Some(raw) = env::var(env_var).ok().filter(|value| !value.is_empty()) else {
        return default_url.to_string();
    };
    let Ok(url) = raw.parse::<reqwest::Url>() else {
        tracing::warn!(
            env = env_var,
            "admin: ignoring token URL override (not a valid URL)"
        );
        return default_url.to_string();
    };
    if is_safe_refresh_url(&url) {
        raw
    } else {
        tracing::warn!(
            env = env_var,
            scheme = url.scheme(),
            host = url.host_str().unwrap_or_default(),
            "admin: ignoring token URL override (only https, or http to loopback, is allowed)"
        );
        default_url.to_string()
    }
}

/// Process-wide client for the OAuth refresh POST; follows a 3xx only to a safe
/// endpoint ([`is_safe_refresh_url`]), closing [`sanitize_token_url`]'s initial-URL gap.
pub(crate) fn token_refresh_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::custom(|attempt| {
                    if attempt.previous().len() >= 10 || !is_safe_refresh_url(attempt.url()) {
                        attempt.error("unsafe or excessive token refresh redirect refused")
                    } else {
                        attempt.follow()
                    }
                }))
                .build()
                .expect("build redirect-hardened token refresh client")
        })
        .clone()
}

pub(crate) fn write_auth_file_atomic(path: &Path, value: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    // Deliberately the no-mkdir entry point: a missing credential directory
    // stays an error rather than being created with umask-default permissions.
    crate::atomic_file::write_private_atomic_in_existing_dir(path, &bytes)
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

fn account_files(dir: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut files = Vec::new();
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
        files.push((name.to_string(), path));
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
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
    account_files(dir).map(|files| {
        files
            .into_iter()
            .map(|(name, path)| AccountConfig {
                name,
                uuid: uuid_for(&path),
                store_entry: true,
                ..Default::default()
            })
            .collect()
    })
}

/// Like [`scan_account_dir`], but propagates a per-file identity-read failure
/// as a whole-scan `Err` instead of silently treating that account as having
/// no identity (which `scan_account_dir` does, via `uuid_for` returning
/// `None` on a read/parse error the same as on a legitimately absent field).
/// Admin cleanup calls this — not `scan_account_dir` — for the fail-closed
/// check of whether a sibling store alias still shares the identity being
/// removed: a corrupted or unreadable sibling file must not be silently
/// treated as "definitely a different identity", since that could let a
/// shared identity's process-lifetime health be cleared while a still-valid
/// (but unreadable-right-now) alias depends on it.
pub fn scan_account_dir_strict(
    dir: &Path,
    uuid_for: impl Fn(&Path) -> Result<Option<String>, ()>,
) -> io::Result<Vec<AccountConfig>> {
    account_files(dir)?
        .into_iter()
        .map(|(name, path)| {
            let uuid = uuid_for(&path).map_err(|()| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("failed to read upstream identity from account file {path:?}"),
                )
            })?;
            Ok(AccountConfig {
                name,
                uuid,
                store_entry: true,
                ..Default::default()
            })
        })
        .collect()
}

/// Resolve a pooled provider's effective account list: its configured
/// `[[accounts]]` when non-empty, otherwise a scan of the store directory
/// (cached by its mtime — see [`scan_cached`]) that enables no-restart account
/// discovery, mirroring the Anthropic pool. The existence probe and the scan
/// both do synchronous filesystem I/O, so they run together on the blocking
/// pool — never a stat or `read_dir` on a runtime worker thread. When
/// `accounts_dir` is genuinely absent
/// (a `NotFound` stat) — the backward-compat deployment that sets
/// `auth = "..._oauth"` but never runs `shunt login` — the scan is short-circuited
/// right after that cheap stat (no `read_dir`, no per-file reads), preserving the
/// near-zero-I/O single-account path (#118). Any *other* stat error (e.g. a
/// permission fault on an existing but unreadable store) is surfaced rather than
/// masked as "no accounts", mirroring `scan_account_dir`'s own `NotFound`-only
/// handling and preserving the pre-#118 guarantee that a broken store is
/// diagnosable. The check runs on every request, so once an account is added
/// (which creates the directory) scanning resumes with no restart. The
/// `read_dir` + per-account UUID reads are cached by the store directory's mtime
/// (see [`scan_cached`]): an unchanged store re-serves the last scan, so
/// steady-state account-list discovery costs one stat and zero credential-file
/// reads, while adding or removing an account changes the directory mtime and so
/// re-scans on the next request — preserving no-restart discovery. (Directory
/// mtime is the invalidation signal, so on a filesystem with coarse mtime
/// resolution a change that shares the cached scan's timestamp goes unnoticed
/// until a later change advances the mtime.)
/// `provider_label` shapes the error text ("codex" / "Claude") and partitions the
/// scan cache (see [`scan_cache`]); the error is returned preformatted so each
/// pool wraps it in its own gateway error type.
pub(crate) async fn resolve_pool_accounts(
    provider_label: &str,
    configured: &[AccountConfig],
    account_scope: &[String],
    store_family: StoreFamily,
    accounts_dir: PathBuf,
    scan: fn() -> io::Result<Vec<AccountConfig>>,
) -> Result<Vec<AccountConfig>, String> {
    let mut accounts = if !configured.is_empty() && account_scope.is_empty() {
        configured.to_vec()
    } else {
        // The stat + scan are both synchronous file I/O, so run the whole thing on
        // the blocking pool — never on a runtime worker thread. The closure still
        // short-circuits on genuine absence (a cheap stat, no `read_dir`); any other
        // stat error is surfaced, not masked as "no accounts".
        let scan_dir = accounts_dir.clone();
        let provider = provider_label.to_string();
        let scanned = tokio::task::spawn_blocking(move || {
            // One stat serves two purposes: the #118 `NotFound` short-circuit (no
            // `read_dir` when the store is genuinely absent) and the cache signal
            // (the store directory's mtime). Reusing it keeps steady-state discovery
            // at one stat and zero credential-file reads.
            let modified = match fs::metadata(&scan_dir) {
                Ok(meta) => meta.modified().ok(),
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(error) => return Err(error),
            };
            scan_cached(&provider, &scan_dir, modified, scan)
        })
        .await
        .map_err(|error| format!("{provider_label} account store scan task failed: {error}"))?
        .map_err(|error| {
            format!(
                "failed to scan {provider_label} account store {}: {error}",
                accounts_dir.display()
            )
        })?;

        if account_scope.is_empty() {
            scanned
        } else {
            let by_name = scanned
                .into_iter()
                .map(|account| (account.name.clone(), account))
                .collect::<HashMap<_, _>>();
            let mut scoped = Vec::with_capacity(account_scope.len() + configured.len());
            for name in account_scope {
                if let Some(account) = by_name.get(name) {
                    scoped.push(account.clone());
                } else {
                    return Err(format!(
                        "{provider_label} account scope references missing store account {name:?}"
                    ));
                }
            }
            scoped.extend_from_slice(configured);
            scoped
        }
    };
    // Assign the store family and resolve any missing inline identities on the
    // blocking pool: the credential-file reads (and mtime stats) never run on a
    // runtime worker thread, and resolved identities are memoized per credential
    // mtime (see [`resolve_inline_identity`]) so the steady-state request path
    // stays off full credential reads and the global env lock.
    let accounts = tokio::task::spawn_blocking(move || {
        for account in &mut accounts {
            account.store_family = Some(store_family);
            if account
                .uuid
                .as_deref()
                .is_none_or(|id| id.trim().is_empty())
            {
                // Store entries already attempted identity extraction during the
                // cached scan; never re-read their credential files here. Inline
                // accounts resolve their identity from a credential file or env
                // token, memoized and mtime-invalidated.
                account.uuid = resolve_inline_identity(store_family, account);
            }
        }
        accounts
    })
    .await
    .map_err(|error| format!("{provider_label} inline identity resolution task failed: {error}"))?;
    Ok(accounts)
}

/// One cached scan result: the accounts a scan produced and the store directory
/// mtime it was produced against. A later request whose stat reports the same
/// mtime reuses `accounts` verbatim.
struct CachedScan {
    modified: SystemTime,
    accounts: Vec<AccountConfig>,
}

/// Process-wide cache of [`scan_account_dir`] results, keyed by
/// `(provider, store directory path)`. Both stores share the map but never read
/// each other's entries: the provider label is part of the key, so even two
/// stores pointed at the same directory (their UUID semantics differ) stay
/// separate. The cache primarily spares the Claude store its per-account UUID
/// reads — a cache hit does zero credential-file reads.
fn scan_cache() -> &'static Mutex<HashMap<(String, PathBuf), CachedScan>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, PathBuf), CachedScan>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Cache key for an inline account's resolved upstream identity. Inline
/// `[[accounts]]` come from static config, so the credential path / env token
/// that yields an account's identity is fixed for the process lifetime, and the
/// identity itself (`shuntAccountUuid` / `chatgpt_account_id`) is stable across
/// token refreshes. The store family is part of the key because the two stores
/// read different identity fields out of the same credential path.
#[derive(Clone, PartialEq, Eq, Hash)]
enum InlineIdentityKey {
    Credentials { family: StoreFamily, path: String },
    TokenEnv { name: String },
}

/// A memoized inline-account identity plus the credential-file mtime it was
/// resolved against (`None` for an env token, which is fixed for the process
/// lifetime).
struct CachedInlineIdentity {
    modified: Option<SystemTime>,
    identity: String,
}

/// Process-wide memo of resolved inline-account identities. This keeps the
/// per-request account-resolution path off full credential-file reads and the
/// global environment lock (`std::env::var`), mirroring the store-scan cache
/// ([`scan_cache`]) and the project's rule against configuration I/O on the
/// request hot path. Only successful (`Some`) resolutions are stored, so a
/// briefly-missing or unreadable credential file is retried on the next request
/// instead of being permanently masked.
fn inline_identity_cache() -> &'static Mutex<HashMap<InlineIdentityKey, CachedInlineIdentity>> {
    static CACHE: OnceLock<Mutex<HashMap<InlineIdentityKey, CachedInlineIdentity>>> =
        OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve an inline account's stable upstream identity from its credential file
/// or environment token, memoized (see [`inline_identity_cache`]). Store entries
/// are skipped — they resolved their identity during the cached store scan and
/// must never be re-read here. The credential file's mtime is the invalidation
/// signal (mirroring [`scan_cache`]): re-provisioning the file to a different
/// account changes its mtime and forces a re-read, so pool identity tracks the
/// credential currently in use rather than pinning to a stale account. An env
/// token has no mtime and memoizes for the process lifetime. On a filesystem
/// with coarse mtime resolution, a change sharing the cached timestamp goes
/// unnoticed until a later change advances the mtime — the same caveat as the
/// store-scan cache. Runs on the blocking pool (see the caller), so its stat and
/// read never touch a runtime worker thread.
fn resolve_inline_identity(store_family: StoreFamily, account: &AccountConfig) -> Option<String> {
    if account.store_entry {
        return None;
    }
    let key = match (
        store_family,
        account.credentials.as_deref(),
        account.token_env.as_deref(),
    ) {
        (family @ (StoreFamily::Claude | StoreFamily::Chatgpt), Some(path), _) => {
            InlineIdentityKey::Credentials {
                family,
                path: path.to_string(),
            }
        }
        (StoreFamily::Chatgpt, None, Some(env_name)) => InlineIdentityKey::TokenEnv {
            name: env_name.to_string(),
        },
        _ => return None,
    };
    let modified = match &key {
        InlineIdentityKey::Credentials { path, .. } => {
            fs::metadata(path).and_then(|meta| meta.modified()).ok()
        }
        InlineIdentityKey::TokenEnv { .. } => None,
    };
    {
        let cache = inline_identity_cache()
            .lock()
            .expect("inline identity cache mutex poisoned");
        if let Some(entry) = cache.get(&key) {
            // Env tokens are fixed for the process lifetime; a credential file is
            // only trusted while we have a current mtime that matches the cached
            // one, so a re-provisioned (or vanished) file re-resolves.
            let fresh = match &key {
                InlineIdentityKey::TokenEnv { .. } => true,
                InlineIdentityKey::Credentials { .. } => {
                    modified.is_some() && entry.modified == modified
                }
            };
            if fresh {
                return Some(entry.identity.clone());
            }
        }
    }
    let resolved = match &key {
        InlineIdentityKey::Credentials {
            family: StoreFamily::Claude,
            path,
        } => crate::auth::claude::store::credential_uuid(Path::new(path)),
        InlineIdentityKey::Credentials {
            family: StoreFamily::Chatgpt,
            path,
        } => crate::auth::codex::store::credential_account_id(Path::new(path)),
        InlineIdentityKey::TokenEnv { name } => env::var(name)
            .ok()
            .and_then(|token| crate::auth::codex::auth::jwt_account_id(&token)),
    };
    if let Some(identity) = &resolved {
        inline_identity_cache()
            .lock()
            .expect("inline identity cache mutex poisoned")
            .insert(
                key,
                CachedInlineIdentity {
                    modified,
                    identity: identity.clone(),
                },
            );
    }
    resolved
}

/// Return the cached scan for `dir` when its stored mtime still matches
/// `modified`; otherwise run `scan`, cache the result against `modified`, and
/// return it. `modified` is the store directory's mtime, sampled *before* the
/// scan so a change racing the scan is caught by the next request rather than
/// masked. When `modified` is `None` (a platform/filesystem that reports no
/// mtime) the cache is bypassed and every call scans, so correctness never
/// depends on a signal we could not read.
fn scan_cached(
    provider: &str,
    dir: &Path,
    modified: Option<SystemTime>,
    scan: impl Fn() -> io::Result<Vec<AccountConfig>>,
) -> io::Result<Vec<AccountConfig>> {
    let Some(modified) = modified else {
        let accounts = scan()?;
        warn_scan_identity_collisions(provider, dir, &accounts);
        return Ok(accounts);
    };
    let key = (provider.to_string(), dir.to_path_buf());
    {
        let cache = scan_cache().lock().expect("scan cache mutex poisoned");
        if let Some(entry) = cache.get(&key) {
            if entry.modified == modified {
                return Ok(entry.accounts.clone());
            }
        }
    }
    // Cache miss (first sight, or the store changed): scan without holding the
    // lock so concurrent hits are never blocked behind this file I/O, then
    // record the result. Concurrent misses each scan and race to insert; the
    // last write wins, and their snapshots may differ if the store changed while
    // they overlapped. That is safe: an entry is served only while its stored
    // mtime equals the mtime this request sampled, so a stale write is re-scanned
    // away as soon as a request observes a different directory mtime.
    let accounts = scan()?;
    warn_scan_identity_collisions(provider, dir, &accounts);
    scan_cache()
        .lock()
        .expect("scan cache mutex poisoned")
        .insert(
            key,
            CachedScan {
                modified,
                accounts: accounts.clone(),
            },
        );
    Ok(accounts)
}

/// One identity collision: the shared identity plus the account names sharing
/// it, as produced by [`crate::config::identity_collisions`].
type IdentityCollision = (String, Vec<String>);

/// `(provider, store directory)` -> the collision set last warned about there.
type LastWarnedCollisions = HashMap<(String, PathBuf), Vec<IdentityCollision>>;

/// Process-wide fingerprint of the last collision set warned about per
/// `(provider, store directory)`, so a store on a filesystem that cannot
/// report mtime (see [`scan_cached`]'s `modified: None` fallback, which scans
/// and thus would otherwise call this on every request) does not re-log the
/// same warning on every single call. Deliberately scoped to this warning
/// only — the request hot path (`collapse_representatives`) still never
/// touches a lock.
fn last_warned_collisions() -> &'static Mutex<LastWarnedCollisions> {
    static LAST_WARNED: OnceLock<Mutex<LastWarnedCollisions>> = OnceLock::new();
    LAST_WARNED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Warn when two store-discovered accounts resolve to the same runtime
/// identity (`crate::accounts::account_identity`). This mirrors the
/// config-load collision warning (`crate::config::identity_collisions`,
/// applied to `[[providers.*.accounts]]`) but for accounts discovered from
/// the on-disk store, which config-load validation never sees.
///
/// `scan_cached` only calls this on a cache miss (the store directory's mtime
/// changed), so on a filesystem that reports mtime this already fires once
/// per change. On a filesystem that cannot report mtime, `scan_cached` scans
/// (and so calls this) on every request; the fingerprint keyed by
/// `(provider, dir)` collapses that back down to once per distinct collision
/// set, without reintroducing a lock on the request hot path (this is the
/// scan/store-discovery boundary, not `collapse_representatives`).
fn warn_scan_identity_collisions(provider: &str, dir: &Path, accounts: &[AccountConfig]) {
    let collisions = crate::config::identity_collisions(provider, accounts);
    let key = (provider.to_string(), dir.to_path_buf());
    {
        let mut last_warned = last_warned_collisions()
            .lock()
            .expect("last-warned collisions mutex poisoned");
        if last_warned.get(&key) == Some(&collisions) {
            return;
        }
        last_warned.insert(key, collisions.clone());
    }
    for (identity, names) in collisions {
        tracing::warn!(
            provider,
            identity,
            accounts = ?names,
            "multiple account names share one upstream identity; the pool will treat them as one account"
        );
    }
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

/// Test-only RAII guard that sets an environment variable on construction and
/// removes it on drop, so a panic between set and cleanup cannot leak the
/// override into a sibling test. Shared by the Claude and Codex store test
/// modules — both drive `SHUNT_*_ACCOUNTS_DIR` — so their cleanup cannot drift.
/// Pair it with each store's `TEST_ENV_LOCK` (declare the guard *after* the lock
/// so it drops first, removing the var while the lock is still held).
#[cfg(test)]
pub(crate) struct EnvVarGuard {
    key: &'static str,
}

#[cfg(test)]
impl EnvVarGuard {
    pub(crate) fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        env::set_var(key, value);
        Self { key }
    }
}

#[cfg(test)]
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        env::remove_var(self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

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
    fn generated_pkce_values_are_urlsafe_and_s256_linked() {
        use sha2::{Digest, Sha256};

        let pkce = generate_pkce();
        assert_eq!(pkce.verifier.len(), 43);
        assert_eq!(pkce.challenge.len(), 43);
        assert_eq!(pkce.state.len(), 43);
        assert_eq!(
            pkce.challenge,
            URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()))
        );
        for value in [&pkce.verifier, &pkce.challenge, &pkce.state] {
            assert!(value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'));
        }
        assert_ne!(pkce.verifier, pkce.state);
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
    fn admin_token_url_override_returns_safe_overrides_and_falls_back_otherwise() {
        // A per-pid var name no other test reads, so no cross-test env race.
        let env_name = format!("SHUNT_TEST_ADMIN_TOKEN_URL_{}", std::process::id());
        let default = "https://auth.example.com/oauth/token";

        // Unset and empty both fall back to the built-in default.
        env::remove_var(&env_name);
        assert_eq!(admin_token_url_override(&env_name, default), default);
        env::set_var(&env_name, "");
        assert_eq!(admin_token_url_override(&env_name, default), default);

        // Safe overrides are honored: https anywhere, or http to loopback.
        env::set_var(&env_name, "https://localhost:9999/token");
        assert_eq!(
            admin_token_url_override(&env_name, default),
            "https://localhost:9999/token"
        );
        env::set_var(&env_name, "http://127.0.0.1:9999/token");
        assert_eq!(
            admin_token_url_override(&env_name, default),
            "http://127.0.0.1:9999/token"
        );

        // A malformed URL, or http to a non-loopback host, is ignored (with a warn)
        // and the default is used — no silent egress of the one-time code.
        env::set_var(&env_name, "not a url");
        assert_eq!(admin_token_url_override(&env_name, default), default);
        env::set_var(&env_name, "http://evil.example.com/token");
        assert_eq!(admin_token_url_override(&env_name, default), default);

        env::remove_var(&env_name);
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
    fn scan_account_dir_strict_returns_identities_when_all_files_read_cleanly() {
        let dir = temp_dir("scan-strict-ok");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("alpha.json"), "{}").unwrap();
        fs::write(dir.join("zeta.json"), "{}").unwrap();

        let accounts =
            scan_account_dir_strict(&dir, |_| Ok(Some("shared-id".to_string()))).unwrap();
        let names: Vec<_> = accounts
            .iter()
            .map(|account| account.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);
        assert!(accounts
            .iter()
            .all(|account| account.uuid.as_deref() == Some("shared-id")));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scan_account_dir_strict_fails_closed_on_a_single_unreadable_file() {
        // A per-file identity read failure (corrupted/unreadable credential
        // file) must abort the whole scan with an `Err`, not silently drop
        // that account to a name-fallback identity — otherwise admin cleanup
        // could conclude a sibling alias does not share the identity being
        // removed when it actually can't tell.
        let dir = temp_dir("scan-strict-fail");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("alpha.json"), "{}").unwrap();
        fs::write(dir.join("broken.json"), "{}").unwrap();

        let result = scan_account_dir_strict(&dir, |path| {
            if path.file_stem().and_then(|name| name.to_str()) == Some("broken") {
                Err(())
            } else {
                Ok(None)
            }
        });
        assert!(result.is_err());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scan_account_dir_strict_missing_dir_is_empty() {
        let dir = temp_dir("scan-strict-missing").join("does-not-exist");
        assert!(scan_account_dir_strict(&dir, |_| Ok(None))
            .unwrap()
            .is_empty());
    }

    fn one_account() -> io::Result<Vec<AccountConfig>> {
        Ok(vec![AccountConfig {
            name: "primary".to_string(),
            ..Default::default()
        }])
    }

    fn uuidless_store_account_with_identity_file() -> io::Result<Vec<AccountConfig>> {
        let dir = temp_dir("pool-store-entry-credential");
        fs::create_dir_all(&dir)?;
        let path = dir.join("primary.json");
        fs::write(&path, r#"{"shuntAccountUuid":"must-not-be-reread"}"#)?;
        Ok(vec![AccountConfig {
            name: "primary".to_string(),
            credentials: Some(path.display().to_string()),
            store_entry: true,
            ..Default::default()
        }])
    }

    fn scan_must_not_run() -> io::Result<Vec<AccountConfig>> {
        panic!("the store scan must be short-circuited");
    }

    #[tokio::test]
    async fn resolve_pool_accounts_does_not_reread_scanned_store_credentials() {
        let dir = temp_dir("pool-store-entry");
        fs::create_dir_all(&dir).unwrap();
        let accounts = resolve_pool_accounts(
            "store-entry-no-reread",
            &[],
            &[],
            StoreFamily::Claude,
            dir.clone(),
            uuidless_store_account_with_identity_file,
        )
        .await
        .unwrap();

        assert_eq!(accounts.len(), 1);
        assert!(accounts[0].store_entry);
        assert_eq!(accounts[0].uuid, None);

        let credential_dir = Path::new(accounts[0].credentials.as_deref().unwrap())
            .parent()
            .unwrap();
        let _ = fs::remove_dir_all(credential_dir);
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn resolve_pool_accounts_memoizes_inline_credential_identity() {
        // An inline account (not a store entry) with a credentials file but no
        // uuid resolves its identity from that file once and memoizes it, so the
        // request hot path never re-reads an unchanged credential file. Configured
        // + empty scope skips the scan entirely (`scan_must_not_run` proves it).
        let dir = temp_dir("pool-inline-memo");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("inline.json");
        fs::write(&path, r#"{"shuntAccountUuid":"inline-identity"}"#).unwrap();
        let configured = vec![AccountConfig {
            name: "inline".to_string(),
            credentials: Some(path.display().to_string()),
            ..Default::default()
        }];

        let first = resolve_pool_accounts(
            "inline-memo",
            &configured,
            &[],
            StoreFamily::Claude,
            dir.clone(),
            scan_must_not_run,
        )
        .await
        .unwrap();
        assert_eq!(first[0].uuid.as_deref(), Some("inline-identity"));

        // Re-reading the unchanged file would still yield the same identity; the
        // memo serves it without touching disk. (Invalidation on change is
        // covered by `resolve_pool_accounts_reresolves_inline_identity_when_...`.)
        let second = resolve_pool_accounts(
            "inline-memo",
            &configured,
            &[],
            StoreFamily::Claude,
            dir.clone(),
            scan_must_not_run,
        )
        .await
        .unwrap();
        assert_eq!(second[0].uuid.as_deref(), Some("inline-identity"));

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn resolve_pool_accounts_reresolves_inline_identity_when_credential_file_changes() {
        // The memo is keyed on the credential file's mtime, so re-provisioning the
        // file to a different account (or removing it) invalidates the cached
        // identity instead of pinning quota/health state to the previous account.
        // Deleting the file makes its mtime unavailable, which the cache treats as
        // "not fresh" and re-resolves — now yielding `None`.
        let dir = temp_dir("pool-inline-invalidate");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("inline.json");
        fs::write(&path, r#"{"shuntAccountUuid":"first-identity"}"#).unwrap();
        let configured = vec![AccountConfig {
            name: "inline".to_string(),
            credentials: Some(path.display().to_string()),
            ..Default::default()
        }];

        let first = resolve_pool_accounts(
            "inline-invalidate",
            &configured,
            &[],
            StoreFamily::Claude,
            dir.clone(),
            scan_must_not_run,
        )
        .await
        .unwrap();
        assert_eq!(first[0].uuid.as_deref(), Some("first-identity"));

        // Remove the credential file: the mtime is gone, so the memo must NOT keep
        // serving the stale identity — the re-resolution reads the missing file and
        // returns `None`.
        fs::remove_file(&path).unwrap();
        let second = resolve_pool_accounts(
            "inline-invalidate",
            &configured,
            &[],
            StoreFamily::Claude,
            dir.clone(),
            scan_must_not_run,
        )
        .await
        .unwrap();
        assert_eq!(second[0].uuid.as_deref(), None);

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn resolve_pool_accounts_short_circuits_when_store_dir_absent() {
        // No configured accounts and no store directory (the backward-compat
        // single-account deployment): the scan is skipped entirely, so a scan fn
        // that would panic is never reached and the list is empty.
        let missing = temp_dir("pool-absent").join("does-not-exist");
        let accounts = resolve_pool_accounts(
            "codex",
            &[],
            &[],
            StoreFamily::Chatgpt,
            missing,
            scan_must_not_run,
        )
        .await
        .unwrap();
        assert!(accounts.is_empty());
    }

    #[tokio::test]
    async fn resolve_pool_accounts_scans_when_store_dir_exists() {
        // Once the store directory exists (an operator added an account), the
        // first request scans and caches it — no-restart discovery is preserved.
        let dir = temp_dir("pool-present");
        fs::create_dir_all(&dir).unwrap();
        let accounts = resolve_pool_accounts(
            "codex",
            &[],
            &[],
            StoreFamily::Chatgpt,
            dir.clone(),
            one_account,
        )
        .await
        .unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].name, "primary");
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn resolve_pool_accounts_prefers_configured_without_scanning() {
        // Configured `[[accounts]]` win outright: the scan is never invoked even
        // when a store directory exists alongside them.
        let dir = temp_dir("pool-configured");
        fs::create_dir_all(&dir).unwrap();
        let configured = one_account().unwrap();
        let accounts = resolve_pool_accounts(
            "codex",
            &configured,
            &[],
            StoreFamily::Chatgpt,
            dir.clone(),
            scan_must_not_run,
        )
        .await
        .unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].name, "primary");
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn resolve_pool_accounts_enforces_scope_and_preserves_whole_store() {
        let dir = temp_dir("pool-scope");
        fs::create_dir_all(&dir).unwrap();
        fn two_accounts() -> io::Result<Vec<AccountConfig>> {
            Ok(vec![
                AccountConfig {
                    name: "primary".to_string(),
                    store_entry: true,
                    ..Default::default()
                },
                AccountConfig {
                    name: "backup".to_string(),
                    store_entry: true,
                    ..Default::default()
                },
            ])
        }

        let whole = resolve_pool_accounts(
            "scope-whole",
            &[],
            &[],
            StoreFamily::Claude,
            dir.clone(),
            two_accounts,
        )
        .await
        .unwrap();
        assert_eq!(account_names(&whole), ["primary", "backup"]);

        let scoped = resolve_pool_accounts(
            "scope-subset",
            &[],
            &["backup".to_string()],
            StoreFamily::Claude,
            dir.clone(),
            two_accounts,
        )
        .await
        .unwrap();
        assert_eq!(account_names(&scoped), ["backup"]);
        assert!(scoped[0].store_entry);
        assert_eq!(scoped[0].store_family, Some(StoreFamily::Claude));
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn resolve_pool_accounts_maps_scan_error_with_label_and_path() {
        // A scan that fails (e.g. an unreadable store) surfaces a provider-labelled
        // error naming the directory, so each pool can wrap it verbatim.
        fn scan_fails() -> io::Result<Vec<AccountConfig>> {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
        }
        let dir = temp_dir("pool-scan-error");
        fs::create_dir_all(&dir).unwrap();
        let error = resolve_pool_accounts(
            "codex",
            &[],
            &[],
            StoreFamily::Chatgpt,
            dir.clone(),
            scan_fails,
        )
        .await
        .unwrap_err();
        assert!(
            error.contains("failed to scan codex account store"),
            "got: {error}"
        );
        assert!(error.contains(&dir.display().to_string()), "got: {error}");
        let _ = fs::remove_dir_all(dir);
    }

    fn account_names(accounts: &[AccountConfig]) -> Vec<&str> {
        accounts
            .iter()
            .map(|account| account.name.as_str())
            .collect()
    }

    #[test]
    fn scan_cached_serves_cache_until_mtime_changes_and_bypasses_without_mtime() {
        // Drive the cache with explicit mtimes (no reliance on filesystem mtime
        // resolution), checking both how often the underlying scan runs and which
        // account set is served. The directory path is unique per test, so the
        // process-wide cache map cannot collide with a sibling test.
        let dir = temp_dir("scan-cached");
        let calls = AtomicUsize::new(0);
        // The scan's result grows with the store: the first mtime yields one
        // account, a later mtime yields two — so an invalidation that re-scans
        // but keeps the stale list is distinguishable from one that refreshes it.
        let scan = || {
            let mut accounts = vec![AccountConfig {
                name: "primary".to_string(),
                ..Default::default()
            }];
            if calls.fetch_add(1, Ordering::Relaxed) >= 1 {
                accounts.push(AccountConfig {
                    name: "secondary".to_string(),
                    ..Default::default()
                });
            }
            Ok(accounts)
        };
        let t1 = UNIX_EPOCH + Duration::from_secs(1_000);

        // First sight: cache miss, scans once, returns the one-account set.
        assert_eq!(
            account_names(&scan_cached("codex", &dir, Some(t1), scan).unwrap()),
            ["primary"]
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        // Unchanged mtime: cache hit — no scan, same set (0 credential-file reads).
        assert_eq!(
            account_names(&scan_cached("codex", &dir, Some(t1), scan).unwrap()),
            ["primary"]
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        // Store changed (mtime advanced): invalidate, re-scan, and the REFRESHED
        // set replaces the stale one — proving invalidation updates the cached
        // value, not merely that it re-scans.
        let t2 = t1 + Duration::from_secs(1);
        assert_eq!(
            account_names(&scan_cached("codex", &dir, Some(t2), scan).unwrap()),
            ["primary", "secondary"]
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);

        // The refreshed set is now what a hit serves (still no further scan).
        assert_eq!(
            account_names(&scan_cached("codex", &dir, Some(t2), scan).unwrap()),
            ["primary", "secondary"]
        );
        assert_eq!(calls.load(Ordering::Relaxed), 2);

        // A different provider at the same path scans its own entry rather than
        // borrowing Codex's — the shared map never serves one provider's scan to
        // another.
        assert_eq!(
            account_names(&scan_cached("Claude", &dir, Some(t2), scan).unwrap()),
            ["primary", "secondary"]
        );
        assert_eq!(calls.load(Ordering::Relaxed), 3);

        // A second Claude call at the same mtime hits Claude's own entry: no
        // re-scan, so the count holds — proving the entry is retained, not shared.
        assert_eq!(
            account_names(&scan_cached("Claude", &dir, Some(t2), scan).unwrap()),
            ["primary", "secondary"]
        );
        assert_eq!(calls.load(Ordering::Relaxed), 3);

        // No mtime signal: the cache is bypassed and every call scans.
        scan_cached("codex", &dir, None, scan).unwrap();
        scan_cached("codex", &dir, None, scan).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 5);
    }

    struct BufferWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl std::io::Write for BufferWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn scan_cached_dedupes_repeated_collision_warnings_without_mtime() {
        // On a filesystem that cannot report mtime, `scan_cached` bypasses the
        // scan cache and scans on every call (see the test above) — but the
        // *warning* about a store-discovered identity collision must still
        // fire once per distinct collision set, not once per call, or a
        // steady-state degraded-mtime deployment would spam its logs on every
        // request.
        let dir = temp_dir("scan-cached-warn-dedup");
        let colliding = || {
            Ok(vec![
                AccountConfig {
                    name: "alias-a".to_string(),
                    uuid: Some("shared-id".to_string()),
                    ..Default::default()
                },
                AccountConfig {
                    name: "alias-b".to_string(),
                    uuid: Some("shared-id".to_string()),
                    ..Default::default()
                },
            ])
        };

        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || BufferWriter {
                buffer: Arc::clone(&writer_output),
            })
            .with_ansi(false)
            .without_time()
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            // Same collision set, called twice: only the first call should warn.
            scan_cached("codex-warn-dedup", &dir, None, colliding).unwrap();
            scan_cached("codex-warn-dedup", &dir, None, colliding).unwrap();
        });
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert_eq!(
            logs.matches("multiple account names share one upstream identity")
                .count(),
            1,
            "got: {logs}"
        );

        // A different collision set must still warn again (not permanently
        // suppressed once one warning has fired for this provider/dir).
        let different_collision = || {
            Ok(vec![
                AccountConfig {
                    name: "alias-c".to_string(),
                    uuid: Some("other-shared-id".to_string()),
                    ..Default::default()
                },
                AccountConfig {
                    name: "alias-d".to_string(),
                    uuid: Some("other-shared-id".to_string()),
                    ..Default::default()
                },
            ])
        };
        output.lock().unwrap().clear();
        let subscriber = tracing_subscriber::fmt()
            .with_writer({
                let writer_output = Arc::clone(&output);
                move || BufferWriter {
                    buffer: Arc::clone(&writer_output),
                }
            })
            .with_ansi(false)
            .without_time()
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            scan_cached("codex-warn-dedup", &dir, None, different_collision).unwrap();
        });
        let logs = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert_eq!(
            logs.matches("multiple account names share one upstream identity")
                .count(),
            1,
            "a changed collision set must warn again: got {logs}"
        );
    }

    static POOL_CACHE_SCAN_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn counting_scan() -> io::Result<Vec<AccountConfig>> {
        POOL_CACHE_SCAN_CALLS.fetch_add(1, Ordering::Relaxed);
        one_account()
    }

    #[tokio::test]
    async fn resolve_pool_accounts_caches_scan_across_unchanged_requests() {
        // With an unchanged store, a second pooled request re-serves the cached
        // scan: the underlying scan runs once, so steady-state discovery does no
        // repeat `read_dir` or per-account reads.
        let dir = temp_dir("pool-cache-hit");
        fs::create_dir_all(&dir).unwrap();
        POOL_CACHE_SCAN_CALLS.store(0, Ordering::Relaxed);

        let first = resolve_pool_accounts(
            "codex",
            &[],
            &[],
            StoreFamily::Chatgpt,
            dir.clone(),
            counting_scan,
        )
        .await
        .unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(POOL_CACHE_SCAN_CALLS.load(Ordering::Relaxed), 1);

        let second = resolve_pool_accounts(
            "codex",
            &[],
            &[],
            StoreFamily::Chatgpt,
            dir.clone(),
            counting_scan,
        )
        .await
        .unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(POOL_CACHE_SCAN_CALLS.load(Ordering::Relaxed), 1);

        let _ = fs::remove_dir_all(dir);
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
