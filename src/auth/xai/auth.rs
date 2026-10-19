//! xAI subscription OAuth credential store.
//!
//! Mirrors [`crate::auth::codex::auth`]: read the shunt-owned credential file fresh on
//! every call, decide expiry from the access token's JWT `exp` (5-minute
//! buffer), refresh against xAI's token endpoint when stale, and write the
//! rotated pair back atomically at `0600`. Token values are never logged — only
//! refresh outcomes. The credential file (`~/.shunt/xai-auth.json`, overridable
//! via `SHUNT_XAI_AUTH_FILE`) is written by `shunt login xai` (see
//! [`super::login`]) and owned solely by shunt.

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex as StdMutex, PoisonError},
    time::{Duration, SystemTime},
};

use serde_json::{json, Value};

use crate::adapters::AdapterError;
use crate::auth::auth_error;
use crate::auth::shared::refresh_lock::{normalized_auth_path, refresh_lock_for};
use crate::auth::shared::{format_iso8601, is_token_valid_at, jwt_claims, write_auth_file_atomic};

/// Public Grok-CLI OAuth client (no secret). xAI's auth server only allows this
/// allowlisted client for the device-code flow. Source: Hermes / OpenCode.
pub(crate) const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub(crate) const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
pub(crate) const DEVICE_CODE_URL: &str = "https://auth.x.ai/oauth2/device/code";
/// Grok-CLI OAuth scopes. Includes `conversations:read`/`conversations:write`
/// alongside `grok-cli:access`/`api:access` so the minted token is accepted by
/// the Grok CLI chat proxy (`cli-chat-proxy.grok.com`), the subscription
/// surface the `grok` provider targets. Mirrors the official Grok CLI
/// (raine/claude-code-proxy `src/providers/grok/auth/login.rs`).
pub(crate) const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access conversations:read conversations:write";
pub(crate) const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XaiCred {
    pub access_token: String,
}

#[derive(Debug, Clone)]
pub struct XaiAuthStore {
    path: PathBuf,
    client: reqwest::Client,
    token_url: String,
    refresh_timeout: Duration,
    /// The command that rewrites this store's file, named in every
    /// "log in again" error ([`DEFAULT_LOGIN_COMMAND`] unless set).
    login_command: String,
}

/// The login that writes the single-account file. A named pool account is
/// re-logged in with `--name` instead (see `auth::resolve_xai_account`).
pub(crate) const DEFAULT_LOGIN_COMMAND: &str = "shunt login xai";

/// Upper bound on one caller's refresh — the wait for its credential file's
/// single-flight lock plus the wait for the refresh — and, separately, on the
/// refresh's own round trip to xAI's token endpoint. A pooled request waits on
/// that refresh before it can fail over, so a token endpoint that accepts the
/// connection and never answers would otherwise hang the request — and every
/// request queued behind it — with no limit (the refresh client sets none). A
/// healthy refresh is one small form POST, so this only ever trips on a
/// stalled endpoint.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

/// Credential files whose current access token xAI rejected (an upstream 401)
/// before its JWT `exp`. The next [`XaiAuthStore::get_valid`] for such a file
/// refreshes instead of handing that token out again until it expires; a
/// successful refresh clears the mark, and a failed one leaves it set.
/// Process-local, like the refresh locks.
static FORCED_REFRESH: LazyLock<StdMutex<ForcedRefresh>> = LazyLock::new(Default::default);

/// The forced-refresh marks, plus the resolved file each marked spelling was
/// found to name. The aliases let a refresh clear every spelling of its file
/// without resolving anyone else's marked paths, which is file-system I/O
/// that a hung mount under another account could stall.
#[derive(Default)]
struct ForcedRefresh {
    marked: HashSet<PathBuf>,
    /// Spelling as marked -> the resolved file it names.
    aliases: HashMap<PathBuf, PathBuf>,
}

fn forced_refresh() -> std::sync::MutexGuard<'static, ForcedRefresh> {
    FORCED_REFRESH
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Make the next `get_valid` for `path` refresh even an unexpired token. The
/// mark is keyed by the resolved file, like the refresh lock, so a store that
/// reaches the same file through another spelling (a symlink) still sees it.
///
/// The path as given is marked synchronously first: that needs no I/O and no
/// await, so it is in place before this returns, and a request that reuses the
/// account at once (a single-account pool keeps a cooled account eligible as a
/// last resort) refreshes instead of resending the rejected token. Resolving
/// the path is file-system I/O on the request path of every xAI 401, so the
/// resolved mark is added from the blocking thread pool ([`record_resolved_mark`]).
/// That task runs to completion whether or not anyone awaits it, so a caller
/// cancelled mid-request (a disconnecting client) cannot lose it. The request
/// path drops the handle; tests await it.
pub(crate) fn force_refresh_next(path: &Path) -> tokio::task::JoinHandle<()> {
    let raw = path.to_path_buf();
    forced_refresh().marked.insert(raw.clone());
    tokio::task::spawn_blocking(move || {
        let resolved = normalized_auth_path(raw.clone());
        record_resolved_mark(raw, resolved);
    })
}

/// Add the resolved mark for `raw`, and remember which file `raw` names — but
/// only while `raw` is still marked. A refresh that already consumed the raw
/// mark replaced the rejected token, so re-adding it would cost a needless
/// second refresh.
fn record_resolved_mark(raw: PathBuf, resolved: PathBuf) {
    let mut forced = forced_refresh();
    if forced.marked.contains(&raw) {
        forced.marked.insert(resolved.clone());
        forced.aliases.insert(raw, resolved);
    }
}

/// Whether `path` is marked by [`force_refresh_next`], as given or resolved.
#[cfg(test)]
pub(crate) fn force_refresh_pending(path: &Path) -> bool {
    forced_refresh_marked(path) || forced_refresh_marked(&normalized_auth_path(path.to_path_buf()))
}

/// Whether exactly `path` is marked by [`force_refresh_next`].
fn forced_refresh_marked(path: &Path) -> bool {
    forced_refresh().marked.contains(path)
}

/// Clear every mark for the file `resolved` after its refresh: the resolved
/// path, the refreshing store's own `configured` spelling, and every spelling
/// recorded as naming `resolved`. No path is resolved here, so other accounts'
/// marks cost nothing. A spelling marked so recently that its alias is not yet
/// recorded may survive, costing at most one extra refresh.
fn clear_forced_refresh(resolved: &Path, configured: &Path) {
    let mut forced = forced_refresh();
    let ForcedRefresh { marked, aliases } = &mut *forced;
    marked.remove(resolved);
    marked.remove(configured);
    aliases.retain(|raw, target| {
        let stale = target == resolved || raw == configured;
        if stale {
            marked.remove(raw);
        }
        !stale
    });
}

/// Tokens as stored on disk under the `tokens` object.
#[derive(Debug, Clone)]
struct TokenSet {
    access_token: String,
    refresh_token: String,
    id_token: Option<String>,
}

/// A token-endpoint response (refresh or device-code exchange).
#[derive(Debug, Clone)]
pub(crate) struct TokenResponse {
    pub access_token: String,
    /// Rotated on every refresh; must be persisted or the next refresh fails.
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
}

impl XaiAuthStore {
    pub fn new(path: PathBuf, client: reqwest::Client) -> Self {
        Self {
            path,
            client,
            token_url: TOKEN_URL.to_string(),
            refresh_timeout: REFRESH_TIMEOUT,
            login_command: DEFAULT_LOGIN_COMMAND.to_string(),
        }
    }

    /// Name `command` in this store's "log in again" errors.
    pub(crate) fn with_login_command(mut self, command: String) -> Self {
        self.login_command = command;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_token_url(
        path: PathBuf,
        client: reqwest::Client,
        token_url: String,
    ) -> Self {
        Self {
            path,
            client,
            token_url,
            refresh_timeout: REFRESH_TIMEOUT,
            login_command: DEFAULT_LOGIN_COMMAND.to_string(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_refresh_timeout(mut self, refresh_timeout: Duration) -> Self {
        self.refresh_timeout = refresh_timeout;
        self
    }

    /// Return a valid access token, refreshing (and persisting the rotated
    /// refresh token) when the stored one is within the 5-minute expiry buffer
    /// or xAI already rejected it ([`force_refresh_next`]).
    pub async fn get_valid(&self) -> Result<XaiCred, AdapterError> {
        // Everything below works on the resolved file — the lock, both reads,
        // the forced-refresh mark, and the writeback — so two stores reaching
        // one file by different spellings (a symlink and its target) share one
        // single-flight refresh, and the rotated pair replaces the real file
        // rather than turning the symlink into a copy that strands the other
        // spelling with the consumed refresh token.
        let (path, tokens) = self.resolve_and_read_off_thread().await?;
        if !self.forced_refresh_requested(&path)
            && is_token_valid_at(&tokens.access_token, SystemTime::now())
        {
            return Ok(XaiCred {
                access_token: tokens.access_token,
            });
        }

        // One deadline bounds this caller's wait: for this file's lock, then for
        // the refresh task. Callers queued behind a stalled refresh then give up
        // when their own bound runs out, instead of each waiting out another
        // full-length attempt in turn, so the pool fails over in bounded time.
        // The token POST itself is not cut short by it (see the task below).
        let deadline = tokio::time::Instant::now() + self.refresh_timeout;

        // Single-flight the refresh per credential file: xAI rotates the refresh
        // token on every refresh, so of two concurrent refreshes of one file the
        // loser would replay a consumed token. A concurrent caller that already
        // holds this file's lock is refreshing right now, so once we acquire it,
        // re-read — the rotated pair it wrote is what we must use, not our stale
        // one. Other accounts' files have their own locks, so a refresh stalled
        // on one pooled account never holds up the next. Cross-process races are
        // out of scope — shunt owns the file and one gateway process is the norm.
        let refreshing = tokio::time::timeout_at(deadline, refresh_lock_for(&path).lock_owned())
            .await
            .map_err(|_| {
                auth_error(
                    "xAI token refresh timed out waiting on a concurrent refresh of this account",
                )
            })?;
        let tokens = self.read_tokens_off_thread(path.clone()).await?;
        // Re-checked under the lock: a caller that refreshed while we waited
        // has already cleared a forced refresh, so its rotated token is reused.
        if !self.forced_refresh_requested(&path)
            && is_token_valid_at(&tokens.access_token, SystemTime::now())
        {
            return Ok(XaiCred {
                access_token: tokens.access_token,
            });
        }

        // Run the refresh + rotated-pair write in a detached task that owns the
        // single-flight guard. `tokio::spawn` keeps running when its JoinHandle
        // is dropped, so a cancelled request (client disconnect) can no longer
        // release the lock mid-write and let another caller re-refresh with the
        // already-consumed refresh token (token-replay race). The lock is held
        // until the write completes, regardless of caller cancellation — or of
        // this caller giving up at its deadline.
        let client = self.client.clone();
        let token_url = self.token_url.clone();
        let login_command = self.login_command.clone();
        let configured = self.path.clone();
        let refresh_timeout = self.refresh_timeout;
        let handle = tokio::spawn(async move {
            let _refreshing = refreshing; // held until the write below completes
                                          // Bounded so a token endpoint that never answers releases the lock,
                                          // but by a full `refresh_timeout` of its own rather than the caller's
                                          // deadline: a caller that spent most of its budget queued must not
                                          // cut off a POST xAI may already have honored, which would consume
                                          // the refresh token and lose the rotated pair. If the caller gives
                                          // up first, this still persists the pair for the next caller.
            let refreshed = tokio::time::timeout(
                refresh_timeout,
                refresh_tokens(&client, &token_url, &tokens.refresh_token, &login_command),
            )
            .await
            .map_err(|_| {
                auth_error("xAI token refresh timed out; the token endpoint did not answer")
            })??;
            // xAI rotates the refresh token on every refresh and consumes the old
            // one. A success that omits refresh_token would leave the consumed
            // token on disk and break the next refresh, so treat it as an invalid
            // response instead of persisting a broken pair.
            let Some(refresh_token) = refreshed.refresh_token.as_deref() else {
                return Err(auth_error(format!(
                    "xAI refresh response missing refresh_token; run {login_command}"
                )));
            };
            let id_token = refreshed.id_token.as_deref().or(tokens.id_token.as_deref());
            write_tokens_off_thread(
                path.clone(),
                refreshed.access_token.clone(),
                refresh_token.to_string(),
                id_token.map(ToOwned::to_owned),
            )
            .await
            .map_err(|error| auth_error(format!("failed to update xAI auth file: {error}")))?;
            clear_forced_refresh(&path, &configured);
            tracing::info!("refreshed xAI OAuth access token");
            Ok::<String, AdapterError>(refreshed.access_token)
        });
        // Dropping the handle on timeout detaches the task; it does not cancel it.
        let access_token = tokio::time::timeout_at(deadline, handle)
            .await
            .map_err(|_| {
                auth_error("xAI token refresh timed out; the token endpoint did not answer in time")
            })?
            .map_err(|error| auth_error(format!("xAI refresh task failed: {error}")))??;
        Ok(XaiCred { access_token })
    }

    /// Whether this store's file is marked for a forced refresh, under its
    /// resolved path or under the path as configured (marked synchronously by
    /// [`force_refresh_next`] before its resolved mark lands).
    fn forced_refresh_requested(&self, resolved: &Path) -> bool {
        forced_refresh_marked(resolved) || forced_refresh_marked(&self.path)
    }

    /// Read the resolved credential file `path`. Errors name the path as
    /// configured, which is the one the operator knows.
    fn read_tokens(&self, path: &Path) -> Result<TokenSet, AdapterError> {
        let value = read_auth_value(path).map_err(|error| {
            auth_error(read_error_message(&self.path, &error, &self.login_command))
        })?;
        parse_tokens(&value).ok_or_else(|| {
            auth_error(format!(
                "xAI auth tokens missing; run {}",
                self.login_command
            ))
        })
    }

    /// Read + parse the credential file on the blocking thread pool so the
    /// synchronous file I/O never stalls the async runtime.
    async fn read_tokens_off_thread(&self, path: PathBuf) -> Result<TokenSet, AdapterError> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.read_tokens(&path))
            .await
            .map_err(|error| auth_error(format!("xAI auth read task failed: {error}")))?
    }

    /// Resolve this store's file ([`normalized_auth_path`]) and read it, in one
    /// trip to the blocking thread pool.
    async fn resolve_and_read_off_thread(&self) -> Result<(PathBuf, TokenSet), AdapterError> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let path = normalized_auth_path(this.path.clone());
            this.read_tokens(&path).map(|tokens| (path, tokens))
        })
        .await
        .map_err(|error| auth_error(format!("xAI auth read task failed: {error}")))?
    }
}

/// Persist the rotated token pair on the blocking thread pool. Same content,
/// path, and atomicity as [`write_tokens`] — only the executing thread differs.
async fn write_tokens_off_thread(
    path: PathBuf,
    access_token: String,
    refresh_token: String,
    id_token: Option<String>,
) -> io::Result<()> {
    tokio::task::spawn_blocking(move || {
        write_tokens(&path, &access_token, &refresh_token, id_token.as_deref())
    })
    .await
    .map_err(|error| io::Error::other(format!("xAI auth write task failed: {error}")))?
}

/// User-facing message for a failed credential-file read. A missing file means
/// "log in"; anything else (EACCES, corrupt JSON) names the real cause so the
/// operator isn't misdirected into a re-login that can't help.
fn read_error_message(path: &Path, error: &io::Error, login_command: &str) -> String {
    if error.kind() == io::ErrorKind::NotFound {
        format!("xAI auth not found; run {login_command}")
    } else {
        format!(
            "xAI auth file {} unreadable: {error}; fix the file or run {login_command}",
            path.display()
        )
    }
}

fn read_auth_value(path: &Path) -> io::Result<Value> {
    serde_json::from_slice(&fs::read(path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn parse_tokens(value: &Value) -> Option<TokenSet> {
    let tokens = value.get("tokens")?;
    Some(TokenSet {
        access_token: tokens.get("access_token")?.as_str()?.to_string(),
        refresh_token: tokens.get("refresh_token")?.as_str()?.to_string(),
        id_token: tokens
            .get("id_token")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
    })
}

/// The xAI user id (the `sub` claim) of an access token. Decoded without
/// signature verification, so it serves display matching only (the dashboard
/// folding a Grok CLI login into the pooled account of the same user) and is
/// never a pool identity.
pub fn jwt_user_id(token: &str) -> Option<String> {
    jwt_claims(token)?
        .get("sub")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// The credential-file JSON: `{"tokens": {...}, "last_refresh": "<ISO8601>"}`.
/// Shared by the legacy single-account file ([`write_tokens`]) and the named
/// account store ([`super::store::store_oauth_tokens`]), so [`XaiAuthStore`]
/// reads and refreshes either one.
pub(crate) fn tokens_value(
    access_token: &str,
    refresh_token: &str,
    id_token: Option<&str>,
) -> Value {
    let mut tokens = serde_json::Map::new();
    tokens.insert("access_token".to_string(), json!(access_token));
    tokens.insert("refresh_token".to_string(), json!(refresh_token));
    if let Some(id_token) = id_token {
        tokens.insert("id_token".to_string(), json!(id_token));
    }
    json!({
        "tokens": Value::Object(tokens),
        "last_refresh": format_iso8601(SystemTime::now()),
    })
}

/// Write the credential file: `{"tokens": {...}, "last_refresh": "<ISO8601>"}`.
/// Creates the parent directory, writes atomically (temp + rename), `0600`.
pub(crate) fn write_tokens(
    path: &Path,
    access_token: &str,
    refresh_token: &str,
    id_token: Option<&str>,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    write_auth_file_atomic(path, &tokens_value(access_token, refresh_token, id_token))
}

pub(crate) fn parse_token_response(value: &Value) -> Option<TokenResponse> {
    Some(TokenResponse {
        access_token: value
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())?
            .to_string(),
        refresh_token: value
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
        id_token: value
            .get("id_token")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
    })
}

/// Refresh the token pair. `login_command` is named in every "log in again"
/// error (see [`XaiAuthStore::with_login_command`]).
pub(crate) async fn refresh_tokens(
    client: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
    login_command: &str,
) -> Result<TokenResponse, AdapterError> {
    let invalid_response =
        || auth_error(format!("invalid xAI refresh response; run {login_command}"));
    let response = client
        .post(token_url)
        .header("accept", "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        // A refused redirect must be distinguishable from a transport failure.
        .map_err(|error| {
            if error.is_redirect() {
                auth_error(format!(
                    "xAI token refresh redirect refused; run {login_command}"
                ))
            } else {
                auth_error(format!("failed to refresh xAI auth; run {login_command}"))
            }
        })?;
    let status = response.status();
    if !status.is_success() {
        tracing::warn!(%status, "xAI token refresh failed");
        return Err(refresh_status_error(status, login_command));
    }
    let text = response.text().await.map_err(|_| invalid_response())?;
    let value = serde_json::from_str::<Value>(&text).map_err(|_| invalid_response())?;
    parse_token_response(&value).ok_or_else(invalid_response)
}

pub(crate) fn refresh_status_error(
    status: reqwest::StatusCode,
    login_command: &str,
) -> AdapterError {
    auth_error(refresh_error_message(status, login_command))
}

/// Map a non-2xx refresh status to a distinct client-facing message. A 403 is a
/// subscription-tier/entitlement gate (re-login won't help — point at the
/// API-key path); 400/401 is a consumed/invalid grant (re-login).
pub(crate) fn refresh_error_message(status: reqwest::StatusCode, login_command: &str) -> String {
    if status == reqwest::StatusCode::FORBIDDEN {
        "xAI OAuth account is not authorized for API access (subscription tier gate); \
         re-logging in will not help — set XAI_API_KEY and use the api-key path, \
         or upgrade your subscription at https://x.ai/grok"
            .to_string()
    } else if status == reqwest::StatusCode::BAD_REQUEST
        || status == reqwest::StatusCode::UNAUTHORIZED
    {
        format!("xAI token refresh rejected (invalid_grant); run {login_command}")
    } else {
        format!("xAI token refresh failed (HTTP {status}); run {login_command}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

    fn jwt(exp: u64) -> String {
        let payload = json!({ "exp": exp });
        format!(
            "x.{}.y",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        )
    }

    fn future_exp() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3_600
    }

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shunt-xai-auth-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir.join("xai-auth.json")
    }

    #[test]
    fn parses_tokens_from_file_value() {
        let value = json!({
            "tokens": {"access_token": "a", "refresh_token": "r", "id_token": "i"},
            "last_refresh": "2026-01-01T00:00:00Z"
        });
        let tokens = parse_tokens(&value).unwrap();
        assert_eq!(tokens.access_token, "a");
        assert_eq!(tokens.refresh_token, "r");
        assert_eq!(tokens.id_token.as_deref(), Some("i"));
    }

    #[test]
    fn missing_refresh_token_fails_to_parse() {
        let value = json!({"tokens": {"access_token": "a"}});
        assert!(parse_tokens(&value).is_none());
    }

    #[test]
    fn write_tokens_round_trips_and_rotates_refresh() {
        let path = temp_path("roundtrip");
        write_tokens(&path, "access-1", "refresh-1", Some("id-1")).unwrap();
        let first = parse_tokens(&read_auth_value(&path).unwrap()).unwrap();
        assert_eq!(first.access_token, "access-1");
        assert_eq!(first.refresh_token, "refresh-1");

        // A subsequent refresh persists the rotated refresh token, replacing it.
        write_tokens(&path, "access-2", "refresh-2", Some("id-1")).unwrap();
        let second = parse_tokens(&read_auth_value(&path).unwrap()).unwrap();
        assert_eq!(second.access_token, "access-2");
        assert_eq!(second.refresh_token, "refresh-2");
        assert_eq!(second.id_token.as_deref(), Some("id-1"));

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn written_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_path("perms");
        write_tokens(&path, "a", "r", None).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn parses_token_endpoint_response() {
        let value = json!({
            "access_token": "new-access",
            "refresh_token": "new-refresh",
            "id_token": "new-id",
            "expires_in": 900
        });
        let parsed = parse_token_response(&value).unwrap();
        assert_eq!(parsed.access_token, "new-access");
        assert_eq!(parsed.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(parsed.id_token.as_deref(), Some("new-id"));

        // A response without an access_token is not a valid success.
        assert!(parse_token_response(&json!({"refresh_token": "r"})).is_none());
    }

    #[test]
    fn read_errors_distinguish_missing_file_from_unreadable() {
        let missing = io::Error::new(io::ErrorKind::NotFound, "no such file");
        assert_eq!(
            read_error_message(Path::new("/tmp/x.json"), &missing, DEFAULT_LOGIN_COMMAND),
            "xAI auth not found; run shunt login xai"
        );

        // EACCES / corrupt JSON name the real cause instead of claiming the
        // file doesn't exist and misdirecting the operator into a re-login.
        let denied = io::Error::new(io::ErrorKind::PermissionDenied, "permission denied");
        let message = read_error_message(Path::new("/tmp/x.json"), &denied, DEFAULT_LOGIN_COMMAND);
        assert!(message.contains("/tmp/x.json"));
        assert!(message.contains("permission denied"));
        assert!(!message.contains("not found"));
    }

    #[test]
    fn refresh_status_errors_are_distinct() {
        // 403 = tier gate: must NOT tell the user to re-login; points at the key.
        let tier = refresh_error_message(reqwest::StatusCode::FORBIDDEN, DEFAULT_LOGIN_COMMAND);
        assert!(tier.contains("tier gate"));
        assert!(tier.contains("XAI_API_KEY"));
        assert!(!tier.contains("run shunt login xai"));

        // 400/401 = invalid grant: tell the user to log in again.
        assert!(
            refresh_error_message(reqwest::StatusCode::BAD_REQUEST, DEFAULT_LOGIN_COMMAND)
                .contains("run shunt login xai")
        );
        assert!(
            refresh_error_message(reqwest::StatusCode::UNAUTHORIZED, DEFAULT_LOGIN_COMMAND)
                .contains("run shunt login xai")
        );

        // Any other status falls back to the generic HTTP-code message.
        let other = refresh_error_message(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            DEFAULT_LOGIN_COMMAND,
        );
        assert!(other.contains("500"));
        assert!(other.contains("run shunt login xai"));
        assert!(!other.contains("tier gate"));
    }

    #[tokio::test]
    async fn refresh_rotates_and_get_valid_writes_back() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": jwt(future_exp()),
                "refresh_token": "rotated-refresh",
                "id_token": "rotated-id",
                "expires_in": 900
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let token_url = format!("{}/token", server.uri());
        let refreshed = refresh_tokens(&client, &token_url, "old-refresh", DEFAULT_LOGIN_COMMAND)
            .await
            .unwrap();
        assert_eq!(refreshed.refresh_token.as_deref(), Some("rotated-refresh"));

        // The rotated refresh token replaces the old one on disk.
        let path = temp_path("writeback");
        write_tokens(&path, "old-access", "old-refresh", None).unwrap();
        let refresh = refreshed.refresh_token.as_deref().unwrap_or("old-refresh");
        write_tokens(
            &path,
            &refreshed.access_token,
            refresh,
            refreshed.id_token.as_deref(),
        )
        .unwrap();
        let stored = parse_tokens(&read_auth_value(&path).unwrap()).unwrap();
        assert_eq!(stored.refresh_token, "rotated-refresh");
        assert!(is_token_valid_at(&stored.access_token, SystemTime::now()));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn concurrent_get_valid_single_flights_the_refresh() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // xAI consumes the refresh token on first use, so a second refresh POST
        // would fail in production — expect exactly one.
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": jwt(future_exp()),
                "refresh_token": "rotated-refresh",
                "expires_in": 900
            })))
            .expect(1)
            .mount(&server)
            .await;

        let file = temp_path("singleflight");
        write_tokens(&file, &jwt(0), "old-refresh", None).unwrap();
        let store = XaiAuthStore::with_token_url(
            file.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
        );

        let (first, second) = tokio::join!(store.get_valid(), store.get_valid());
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(first, second);
        assert!(is_token_valid_at(&first.access_token, SystemTime::now()));

        // The loser re-read the winner's rotated pair instead of replaying the
        // consumed token; the file holds the rotated refresh token.
        let stored = parse_tokens(&read_auth_value(&file).unwrap()).unwrap();
        assert_eq!(stored.refresh_token, "rotated-refresh");
        server.verify().await;
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// xAI can reject a token before its `exp` (an upstream 401). Once that
    /// file is marked, the next `get_valid` refreshes instead of handing the
    /// rejected token out again, and a successful refresh clears the mark.
    #[tokio::test]
    async fn a_forced_refresh_replaces_a_token_that_has_not_expired() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let fresh = jwt(future_exp() + 60);
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": fresh,
                "refresh_token": "rotated-refresh",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let file = temp_path("forced");
        let rejected = jwt(future_exp());
        write_tokens(&file, &rejected, "old-refresh", None).unwrap();
        let store = XaiAuthStore::with_token_url(
            file.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
        );
        // Unmarked, the unexpired token is used as is.
        assert_eq!(store.get_valid().await.unwrap().access_token, rejected);

        force_refresh_next(&file).await.unwrap();
        assert_eq!(store.get_valid().await.unwrap().access_token, fresh);
        // The mark is cleared: the refreshed token is reused without a second
        // refresh (the mock expects exactly one).
        assert!(!force_refresh_pending(&file));
        assert_eq!(store.get_valid().await.unwrap().access_token, fresh);

        server.verify().await;
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[tokio::test]
    async fn refresh_without_rotated_token_is_rejected_and_not_persisted() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // xAI consumes the old refresh token on every refresh, so a success
        // that omits refresh_token must not overwrite the stored pair.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": jwt(4_000_000_000),
                "expires_in": 900
            })))
            .mount(&server)
            .await;

        let file = temp_path("norotate");
        write_tokens(&file, &jwt(0), "old-refresh", None).unwrap();
        let store = XaiAuthStore::with_token_url(
            file.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
        );

        let error = store.get_valid().await.unwrap_err();
        assert_eq!(error.message, "authentication failed");
        // The stored pair is untouched — nothing was persisted.
        let stored = parse_tokens(&read_auth_value(&file).unwrap()).unwrap();
        assert_eq!(stored.refresh_token, "old-refresh");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[tokio::test]
    async fn refresh_maps_403_to_tier_gate_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let token_url = format!("{}/token", server.uri());
        // A 403 refresh surfaces an authentication error (the distinct tier-gate
        // wording is asserted on refresh_error_message above).
        assert!(
            refresh_tokens(&client, &token_url, "r", DEFAULT_LOGIN_COMMAND)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn concurrent_refreshes_of_two_accounts_write_only_their_own_files() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        fn jwt_for(sub: &str) -> String {
            let payload = json!({ "exp": future_exp(), "sub": sub });
            format!(
                "x.{}.y",
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
            )
        }

        // xAI consumes a refresh token on use: each account's grant must be
        // redeemed exactly once, and each rotated pair must land in its own file.
        let server = MockServer::start().await;
        let access_a = jwt_for("a");
        let access_b = jwt_for("b");
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("refresh_token=refresh-a"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": access_a,
                "refresh_token": "refresh-a2"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("refresh_token=refresh-b"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": access_b,
                "refresh_token": "refresh-b2"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let file_a = temp_path("pair-a");
        let file_b = temp_path("pair-b");
        write_tokens(&file_a, &jwt(0), "refresh-a", None).unwrap();
        write_tokens(&file_b, &jwt(0), "refresh-b", None).unwrap();
        let token_url = format!("{}/token", server.uri());
        let store_a =
            XaiAuthStore::with_token_url(file_a.clone(), reqwest::Client::new(), token_url.clone());
        let store_b =
            XaiAuthStore::with_token_url(file_b.clone(), reqwest::Client::new(), token_url);

        let (a, b) = tokio::join!(store_a.get_valid(), store_b.get_valid());
        assert_eq!(a.unwrap().access_token, access_a);
        assert_eq!(b.unwrap().access_token, access_b);
        let stored_a = parse_tokens(&read_auth_value(&file_a).unwrap()).unwrap();
        let stored_b = parse_tokens(&read_auth_value(&file_b).unwrap()).unwrap();
        assert_eq!(stored_a.refresh_token, "refresh-a2");
        assert_eq!(stored_b.refresh_token, "refresh-b2");
        server.verify().await;
        let _ = std::fs::remove_dir_all(file_a.parent().unwrap());
        let _ = std::fs::remove_dir_all(file_b.parent().unwrap());
    }

    #[tokio::test]
    async fn plain_client_follows_the_307_so_the_refusal_test_can_tell_clients_apart() {
        use axum::body::to_bytes;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Control for the pooled-account redirect test in `auth/mod.rs`: with a
        // client that follows redirects, the 307 hop is attempted and fails at
        // DNS (`.invalid` never resolves). That failure is a transport error,
        // not a refused redirect, so the body must not carry the refusal text.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("location", "http://refresh-thief.invalid/token"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let file = temp_path("plain-redirect");
        write_tokens(&file, &jwt(0), "old-refresh", None).unwrap();
        let store = XaiAuthStore::with_token_url(
            file.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
        );

        let error = store.get_valid().await.unwrap_err();
        let bytes = to_bytes(error.response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(!body.contains("redirect refused"), "got: {body}");
        assert!(body.contains("failed to refresh xAI auth"), "got: {body}");
        server.verify().await;
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// Wait until `server` has received a request whose body contains `needle`,
    /// so a test knows a refresh is in flight before it races a second one.
    async fn wait_for_request(server: &wiremock::MockServer, needle: &str) {
        for _ in 0..200 {
            let received = server.received_requests().await.unwrap_or_default();
            if received
                .iter()
                .any(|request| String::from_utf8_lossy(&request.body).contains(needle))
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("no request containing {needle:?} reached the mock token endpoint");
    }

    #[tokio::test]
    async fn a_stalled_refresh_does_not_block_another_accounts_refresh() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Pooled accounts fail over to each other, so one account's refresh
        // hanging at the token endpoint must not hold up a different account's
        // refresh: the single-flight lock is per credential file, not global.
        let server = MockServer::start().await;
        let access_b = jwt(future_exp());
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("refresh_token=stalled-a"))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(60)))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("refresh_token=refresh-b"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": access_b,
                "refresh_token": "refresh-b2"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let file_a = temp_path("stalled-a");
        let file_b = temp_path("unblocked-b");
        write_tokens(&file_a, &jwt(0), "stalled-a", None).unwrap();
        write_tokens(&file_b, &jwt(0), "refresh-b", None).unwrap();
        let token_url = format!("{}/token", server.uri());
        let store_a =
            XaiAuthStore::with_token_url(file_a.clone(), reqwest::Client::new(), token_url.clone());
        let store_b =
            XaiAuthStore::with_token_url(file_b.clone(), reqwest::Client::new(), token_url);

        let stalled = tokio::spawn(async move { store_a.get_valid().await });
        wait_for_request(&server, "refresh_token=stalled-a").await;

        let b = tokio::time::timeout(std::time::Duration::from_secs(5), store_b.get_valid())
            .await
            .expect("account b's refresh waited on account a's stalled refresh");
        assert_eq!(b.unwrap().access_token, access_b);
        stalled.abort();
        let _ = std::fs::remove_dir_all(file_a.parent().unwrap());
        let _ = std::fs::remove_dir_all(file_b.parent().unwrap());
    }

    #[tokio::test]
    async fn a_refresh_that_never_answers_times_out_and_frees_its_lock() {
        use axum::body::to_bytes;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // The refresh client sets no timeout, so the store bounds the round
        // trip itself: a token endpoint that accepts the connection and never
        // answers must fail the refresh and release the file's lock, so the
        // pool can cool the account down and fail over.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(60)))
            .mount(&server)
            .await;

        let file = temp_path("never-answers");
        write_tokens(&file, &jwt(0), "never-answered", None).unwrap();
        let store = XaiAuthStore::with_token_url(
            file.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
        )
        .with_refresh_timeout(std::time::Duration::from_millis(200));

        let first = tokio::time::timeout(std::time::Duration::from_secs(5), store.get_valid())
            .await
            .expect("the refresh hung past its time limit");
        let bytes = to_bytes(first.unwrap_err().response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("timed out"), "got: {body}");

        let second = tokio::time::timeout(std::time::Duration::from_secs(5), store.get_valid())
            .await
            .expect("the timed-out refresh kept holding its lock");
        assert!(second.is_err());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// Two pooled accounts can reach one credential file by different
    /// spellings (a symlink and its target). A refresh through the symlink must
    /// land in the real file, leaving the symlink a symlink, so the other
    /// spelling reads the rotated pair instead of the consumed refresh token;
    /// and a 401 mark set through either spelling is seen through both.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_refresh_through_a_symlink_updates_the_real_file_for_both_spellings() {
        use std::os::unix::fs::PermissionsExt;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let fresh = jwt(future_exp());
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": fresh,
                "refresh_token": "rotated-refresh",
            })))
            // The symlink's refresh, then the forced one through the real path.
            .expect(2)
            .mount(&server)
            .await;

        let real = temp_path("symlink-real");
        write_tokens(&real, &jwt(0), "old-refresh", None).unwrap();
        let link = temp_path("symlink-link");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let token_url = format!("{}/token", server.uri());
        let via_link =
            XaiAuthStore::with_token_url(link.clone(), reqwest::Client::new(), token_url.clone());
        let via_real =
            XaiAuthStore::with_token_url(real.clone(), reqwest::Client::new(), token_url);

        assert_eq!(via_link.get_valid().await.unwrap().access_token, fresh);
        let link_type = std::fs::symlink_metadata(&link).unwrap().file_type();
        assert!(link_type.is_symlink(), "the refresh replaced the symlink");
        let stored = parse_tokens(&read_auth_value(&real).unwrap()).unwrap();
        assert_eq!(stored.refresh_token, "rotated-refresh");
        let mode = std::fs::metadata(&real).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // The real spelling reuses the rotated token without refreshing again.
        assert_eq!(via_real.get_valid().await.unwrap().access_token, fresh);

        force_refresh_next(&link).await.unwrap();
        assert!(force_refresh_pending(&real));
        assert_eq!(via_real.get_valid().await.unwrap().access_token, fresh);
        assert!(!force_refresh_pending(&link));

        server.verify().await;
        let _ = std::fs::remove_dir_all(real.parent().unwrap());
        let _ = std::fs::remove_dir_all(link.parent().unwrap());
    }

    /// Callers queued on a file's lock behind a stalled refresh are bounded by
    /// their own deadline, not handed a fresh full-length attempt each once the
    /// stalled one gives up — which would delay failover by N x the bound.
    #[tokio::test]
    async fn callers_queued_behind_a_stalled_refresh_keep_their_own_deadline() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_delay(std::time::Duration::from_secs(60)))
            .mount(&server)
            .await;

        let file = temp_path("queued-deadline");
        write_tokens(&file, &jwt(0), "stalled-refresh", None).unwrap();
        let bound = std::time::Duration::from_millis(300);
        let store = XaiAuthStore::with_token_url(
            file.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
        )
        .with_refresh_timeout(bound);

        let first = {
            let store = store.clone();
            tokio::spawn(async move { store.get_valid().await })
        };
        wait_for_request(&server, "refresh_token=stalled-refresh").await;

        let started = std::time::Instant::now();
        let queued: Vec<_> = (0..5)
            .map(|_| {
                let store = store.clone();
                tokio::spawn(async move { store.get_valid().await })
            })
            .collect();
        for caller in queued {
            assert!(caller.await.unwrap().is_err());
        }
        // Five serialized attempts would take at least 5 x 300 ms.
        let elapsed = started.elapsed();
        assert!(elapsed < bound * 4, "queued callers took {elapsed:?}");
        assert!(first.await.unwrap().is_err());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// A caller that spent most of its budget queued gives up before a slow
    /// refresh answers, but the refresh is not cut short with it: xAI consumes
    /// the old refresh token once it honors the POST, so the rotated pair must
    /// still be persisted for the next caller, which then needs no new POST.
    #[tokio::test]
    async fn a_caller_giving_up_mid_refresh_does_not_lose_the_rotated_pair() {
        use std::time::Duration;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let fresh = jwt(future_exp());
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(700))
                    .set_body_json(json!({
                        "access_token": fresh,
                        "refresh_token": "rotated-refresh",
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let file = temp_path("caller-gives-up");
        write_tokens(&file, &jwt(0), "old-refresh", None).unwrap();
        let store = XaiAuthStore::with_token_url(
            file.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
        )
        .with_refresh_timeout(Duration::from_secs(1));

        // Hold the file's lock so the caller spends ~600 ms of its 1 s queued,
        // leaving less than the endpoint's 700 ms answer.
        let held = refresh_lock_for(&normalized_auth_path(file.clone()))
            .lock_owned()
            .await;
        let caller = {
            let store = store.clone();
            tokio::spawn(async move { store.get_valid().await })
        };
        tokio::time::sleep(Duration::from_millis(600)).await;
        drop(held);

        let error = caller.await.unwrap().unwrap_err();
        let bytes = axum::body::to_bytes(error.response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("timed out"));

        // The detached refresh still lands the rotated pair.
        let mut stored = None;
        for _ in 0..300 {
            let tokens = parse_tokens(&read_auth_value(&file).unwrap()).unwrap();
            if tokens.refresh_token == "rotated-refresh" {
                stored = Some(tokens);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            stored.expect("the rotated pair was lost").access_token,
            fresh
        );
        // The next caller reuses it without another POST (the mock expects one).
        assert_eq!(store.get_valid().await.unwrap().access_token, fresh);

        server.verify().await;
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// The request path never awaits the mark, and a caller dropped right after
    /// asking for it (a disconnecting client) must not lose it: the blocking
    /// task still resolves the path and records the mark.
    #[tokio::test]
    async fn a_forced_refresh_mark_survives_its_caller_going_away() {
        let file = temp_path("mark-detached");
        write_tokens(&file, &jwt(future_exp()), "refresh", None).unwrap();

        drop(force_refresh_next(&file));

        let mut marked = false;
        for _ in 0..200 {
            if force_refresh_pending(&file) {
                marked = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(marked, "the mark was lost with its dropped handle");
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// The mark is in place before `force_refresh_next` returns, so a request
    /// that reuses the account at once refreshes even though the resolved mark
    /// from the blocking task may not have landed yet.
    #[tokio::test]
    async fn a_get_valid_right_after_the_mark_refreshes_without_awaiting_it() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let fresh = jwt(future_exp() + 60);
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": fresh,
                "refresh_token": "rotated-refresh",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let file = temp_path("mark-immediate");
        write_tokens(&file, &jwt(future_exp()), "old-refresh", None).unwrap();
        let store = XaiAuthStore::with_token_url(
            file.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
        );

        let _pending = force_refresh_next(&file);
        assert_eq!(store.get_valid().await.unwrap().access_token, fresh);

        server.verify().await;
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// The resolved mark lands from a blocking task, possibly after a refresh
    /// already consumed the raw mark. It must not re-add a mark that refresh
    /// cleared, which would cost a needless second refresh.
    #[test]
    fn a_resolved_mark_landing_after_its_refresh_is_not_re_added() {
        let raw = temp_path("late-alias");
        let resolved = raw.with_file_name("resolved.json");
        forced_refresh().marked.insert(raw.clone());
        // The refresh consumes the raw mark before the blocking task resolves it.
        clear_forced_refresh(&resolved, &raw);

        record_resolved_mark(raw.clone(), resolved.clone());

        assert!(!forced_refresh_marked(&raw));
        assert!(!forced_refresh_marked(&resolved));
        assert!(!forced_refresh().aliases.contains_key(&raw));
    }

    /// Clearing after a refresh touches only its own file's marks: another
    /// account's mark, even under a directory that cannot be resolved, is left
    /// as it is and does not hold the refresh up.
    #[tokio::test]
    async fn a_refresh_clears_only_its_own_files_marks() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let fresh = jwt(future_exp() + 60);
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": fresh,
                "refresh_token": "rotated-refresh",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let unrelated = temp_path("unresolvable").join("missing").join("other.json");
        force_refresh_next(&unrelated).await.unwrap();
        let file = temp_path("own-marks");
        write_tokens(&file, &jwt(future_exp()), "old-refresh", None).unwrap();
        force_refresh_next(&file).await.unwrap();
        let store = XaiAuthStore::with_token_url(
            file.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
        );

        let refreshed = tokio::time::timeout(std::time::Duration::from_secs(5), store.get_valid())
            .await
            .expect("the refresh stalled on another account's mark");
        assert_eq!(refreshed.unwrap().access_token, fresh);
        assert!(!force_refresh_pending(&file));
        assert!(forced_refresh_marked(&unrelated));

        clear_forced_refresh(&unrelated, &unrelated);
        server.verify().await;
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }
}
