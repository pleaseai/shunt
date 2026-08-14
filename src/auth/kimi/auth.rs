//! Kimi Code subscription OAuth credential store.
//!
//! Mirrors [`crate::auth::claude::auth`]: accounts are named, so each caller
//! constructs a [`KimiAuthStore`] for a specific account's file
//! (`~/.shunt/accounts/kimi/<name>.json`, see [`super::store`]), reads it fresh
//! on every call, decides expiry from the stored absolute `expiresAt` (5-minute
//! buffer — Kimi's access token is opaque, not a JWT, so expiry is tracked
//! out-of-band from `expires_in` at issue/refresh time rather than decoded),
//! refreshes against Kimi's token endpoint when stale, and writes the rotated
//! pair back atomically at `0600`, preserving the account's `deviceId`.
//!
//! *** CRITICAL: Kimi's token endpoint returns HTTP 400 for the ordinary,
//! non-terminal `authorization_pending` device-poll response, and HTTP 400 for
//! a dead refresh token's `invalid_grant` too — the two are indistinguishable
//! by status code alone. Never branch on the HTTP status before parsing the
//! body: always attempt to parse a token or an OAuth error envelope from the
//! body first, and only fall back to a bare-status message when the body
//! itself carries neither. See [`super::login`] for the device flow, which
//! has the same requirement while polling.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};

use crate::adapters::AdapterError;
use crate::auth::auth_error;
use crate::auth::shared::write_auth_file_atomic;

/// shunt's Kimi Code OAuth client id (no secret — public device-flow client,
/// measured against the live `auth.kimi.com` device-authorization endpoint).
pub(crate) const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
pub(crate) const TOKEN_URL: &str = "https://auth.kimi.com/api/oauth/token";
pub(crate) const DEVICE_CODE_URL: &str = "https://auth.kimi.com/api/oauth/device_authorization";
pub(crate) const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

const EXPIRY_BUFFER: Duration = Duration::from_secs(5 * 60);
/// Fallback lifetime when a token response omits `expires_in` (matches the
/// Claude store's fallback of one hour).
const DEFAULT_EXPIRES_IN_SECS: i64 = 3600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KimiCred {
    pub access_token: String,
    /// The account's persisted `X-Msh-Device-Id`, when the account file has
    /// one. `None` only for a file written before this field existed.
    pub device_id: Option<String>,
}

#[derive(Clone)]
pub struct KimiAuthStore {
    path: PathBuf,
    client: reqwest::Client,
    token_url: String,
}

/// In-process single-flight for the refresh path. Stores are constructed per
/// request, so the lock is shared across independent instances — mirrors
/// Claude's and xAI's stores (one global lock, not one per account: refreshes
/// across different Kimi accounts serialize behind each other, which is the
/// same trade-off those stores already make). The refresh task owns the guard
/// through atomic writeback so a cancelled caller cannot strand a
/// possibly-consumed refresh token.
static REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    expires_at_ms: i64,
    device_id: Option<String>,
}

impl Tokens {
    fn from_value(value: &Value) -> Option<Self> {
        let oauth = value.get("kimiOauth")?;
        Some(Tokens {
            access_token: oauth.get("accessToken")?.as_str()?.to_string(),
            refresh_token: oauth
                .get("refreshToken")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned),
            expires_at_ms: oauth.get("expiresAt").and_then(Value::as_i64).unwrap_or(0),
            device_id: value
                .get("deviceId")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned),
        })
    }

    fn is_valid_at(&self, now: SystemTime) -> bool {
        let now_ms = now
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        self.expires_at_ms > now_ms + EXPIRY_BUFFER.as_millis() as i64
    }
}

/// A parsed Kimi token-endpoint success response (device exchange or refresh).
#[derive(Debug, Clone)]
pub(crate) struct TokenResponse {
    pub access_token: String,
    /// Kimi's refresh-rotation behavior on a refresh grant is unmeasured, so a
    /// response that omits this is treated leniently (existing refresh token
    /// reused — see `refresh_and_write_back`), not rejected like xAI's
    /// always-rotates store does.
    pub refresh_token: Option<String>,
    pub expires_in: Option<i64>,
}

impl KimiAuthStore {
    pub fn new(path: PathBuf, client: reqwest::Client) -> Self {
        Self {
            path,
            client,
            token_url: TOKEN_URL.to_string(),
        }
    }

    #[cfg(test)]
    fn with_token_url(path: PathBuf, client: reqwest::Client, token_url: String) -> Self {
        Self {
            path,
            client,
            token_url,
        }
    }

    /// Return a valid access token (+ the account's device id), refreshing
    /// (and persisting the rotated pair) when the stored one is within the
    /// 5-minute expiry buffer.
    pub async fn get_valid(&self) -> Result<KimiCred, AdapterError> {
        let tokens = self.read_tokens_off_thread().await?;
        if tokens.is_valid_at(SystemTime::now()) {
            return Ok(KimiCred {
                access_token: tokens.access_token,
                device_id: tokens.device_id,
            });
        }

        // Single-flight the refresh: a concurrent caller that already holds the
        // lock is refreshing right now, so once we acquire it, re-read — the
        // pair it wrote is what we must use, not our stale one.
        let refreshing = REFRESH_LOCK.lock().await;
        let tokens = self.read_tokens_off_thread().await?;
        if tokens.is_valid_at(SystemTime::now()) {
            return Ok(KimiCred {
                access_token: tokens.access_token,
                device_id: tokens.device_id,
            });
        }
        self.refresh_and_write_back(tokens, refreshing).await
    }

    async fn refresh_and_write_back(
        &self,
        tokens: Tokens,
        refreshing: tokio::sync::MutexGuard<'static, ()>,
    ) -> Result<KimiCred, AdapterError> {
        let refresh_token = tokens.refresh_token.ok_or_else(|| {
            auth_error("no Kimi refresh token on file; run shunt login kimi --name <account-name>")
        })?;
        let device_id = tokens.device_id.unwrap_or_default();

        // The detached task owns both the single-flight guard and the critical
        // refresh + writeback sequence, same as Claude's store: dropping the
        // caller's future cannot strand a possibly-consumed refresh token
        // in-flight after the write completes.
        let client = self.client.clone();
        let token_url = self.token_url.clone();
        let path = self.path.clone();
        let handle = tokio::spawn(async move {
            let _refreshing = refreshing;
            let refreshed = refresh_tokens(&client, &token_url, &refresh_token, &device_id).await?;
            // Kimi's rotation behavior on refresh is unmeasured; be lenient like
            // Claude and reuse the existing refresh token when the response
            // omits one, rather than rejecting the response as invalid like
            // xAI's always-rotates store does.
            let refresh_token = refreshed.refresh_token.clone().unwrap_or(refresh_token);
            let expires_at_ms = expires_at_ms(refreshed.expires_in, SystemTime::now());
            let access_token = refreshed.access_token.clone();
            write_back_off_thread(path, access_token.clone(), refresh_token, expires_at_ms)
                .await
                .map_err(|error| {
                    auth_error(format!("failed to update Kimi account file: {error}"))
                })?;
            tracing::info!("refreshed Kimi OAuth access token");
            let device_id = if device_id.is_empty() {
                None
            } else {
                Some(device_id)
            };
            Ok::<KimiCred, AdapterError>(KimiCred {
                access_token,
                device_id,
            })
        });
        handle
            .await
            .map_err(|error| auth_error(format!("Kimi refresh task failed: {error}")))?
    }

    fn read_tokens(&self) -> Result<Tokens, AdapterError> {
        let value = read_file(&self.path)
            .map_err(|error| auth_error(read_error_message(&self.path, &error)))?;
        Tokens::from_value(&value).ok_or_else(|| {
            auth_error(format!(
                "no kimiOauth tokens in {}; run shunt login kimi --name <account-name>",
                self.path.display()
            ))
        })
    }

    /// Read + parse the account file on the blocking thread pool so the
    /// synchronous file I/O never stalls the async runtime.
    async fn read_tokens_off_thread(&self) -> Result<Tokens, AdapterError> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.read_tokens())
            .await
            .map_err(|error| auth_error(format!("Kimi auth read task failed: {error}")))?
    }
}

/// User-facing message for a failed account-file read. A missing file means
/// "log in"; anything else (EACCES, corrupt JSON) names the real cause so the
/// operator isn't misdirected into a re-login that can't help.
fn read_error_message(path: &Path, error: &io::Error) -> String {
    if error.kind() == io::ErrorKind::NotFound {
        "Kimi account not found; run shunt login kimi --name <account-name>".to_string()
    } else {
        format!(
            "Kimi account file {} unreadable: {error}; fix the file or run shunt login kimi --name <account-name>",
            path.display()
        )
    }
}

fn read_file(path: &Path) -> io::Result<Value> {
    serde_json::from_slice(&fs::read(path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Persist the rotated token pair, preserving every other field in the file
/// (in particular the account's `deviceId`) — same shape as
/// `claude::auth::write_back`.
fn write_back(
    path: &Path,
    access_token: &str,
    refresh_token: &str,
    expires_at_ms: i64,
) -> io::Result<()> {
    let mut value = read_file(path)?;
    let oauth = value
        .get_mut("kimiOauth")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "kimiOauth missing"))?;
    oauth.insert("accessToken".to_string(), json!(access_token));
    oauth.insert("refreshToken".to_string(), json!(refresh_token));
    oauth.insert("expiresAt".to_string(), json!(expires_at_ms));
    write_auth_file_atomic(path, &value)
}

/// Persist the refreshed pair on Tokio's blocking pool. The on-disk content
/// and atomic write semantics remain those of [`write_back`].
async fn write_back_off_thread(
    path: PathBuf,
    access_token: String,
    refresh_token: String,
    expires_at_ms: i64,
) -> io::Result<()> {
    tokio::task::spawn_blocking(move || {
        write_back(&path, &access_token, &refresh_token, expires_at_ms)
    })
    .await
    .map_err(|error| io::Error::other(format!("Kimi auth write task failed: {error}")))?
}

/// Compute an absolute expiry (ms since epoch) from a token response's
/// `expires_in` (seconds), falling back to [`DEFAULT_EXPIRES_IN_SECS`] when
/// absent.
pub(crate) fn expires_at_ms(expires_in: Option<i64>, now: SystemTime) -> i64 {
    let now_ms = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    now_ms + expires_in.unwrap_or(DEFAULT_EXPIRES_IN_SECS) * 1000
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
        expires_in: value.get("expires_in").and_then(Value::as_i64),
    })
}

pub(crate) async fn refresh_tokens(
    client: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
    device_id: &str,
) -> Result<TokenResponse, AdapterError> {
    let mut request = client
        .post(token_url)
        .header("accept", "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token),
        ]);
    for (name, value) in msh_headers(device_id) {
        request = request.header(name, value);
    }
    let response = request.send().await.map_err(|_| {
        auth_error(
            "failed to reach Kimi token endpoint; run shunt login kimi --name <account-name>",
        )
    })?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    // See the module doc comment: never gate on `status` before parsing —
    // a 400 here may be a normal, parseable `invalid_grant` envelope.
    let value: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => {
            return Err(auth_error(format!(
            "Kimi token refresh failed (HTTP {status}); run shunt login kimi --name <account-name>"
        )))
        }
    };
    if let Some(parsed) = parse_token_response(&value) {
        return Ok(parsed);
    }
    Err(refresh_error(&value, status))
}

/// Classify a non-success (or success-shaped-but-tokenless) refresh body.
/// `invalid_grant` always means the refresh token is dead — the message must
/// point at re-login by name, not report a bare HTTP status (that would
/// mistranslate a re-login-required failure into an opaque status code, the
/// bug this store must not reproduce — see the module doc comment). Only when
/// the body carries no parseable `error` field at all does this fall back to
/// naming the HTTP status.
fn refresh_error(body: &Value, status: reqwest::StatusCode) -> AdapterError {
    match body.get("error").and_then(Value::as_str) {
        Some("invalid_grant") => auth_error(
            "Kimi refresh token is no longer valid (invalid_grant); run shunt login kimi --name <account-name> again",
        ),
        Some(code) => {
            let description = body
                .get("error_description")
                .and_then(Value::as_str)
                .unwrap_or("");
            if description.is_empty() {
                auth_error(format!("Kimi token refresh failed ({code})"))
            } else {
                auth_error(format!("Kimi token refresh failed ({code}): {description}"))
            }
        }
        None => auth_error(format!(
            "Kimi token refresh failed (HTTP {status}); run shunt login kimi --name <account-name>"
        )),
    }
}

/// The five `X-Msh-*` headers Kimi requires on every request to
/// `auth.kimi.com` (device authorization + token endpoint) and to the Kimi
/// Code API base (`api.kimi.com`), identifying shunt as the client.
/// `device_id` is a UUID generated once at `shunt login kimi --name <name>`
/// and persisted in the account file, so it stays stable across refreshes and
/// outbound requests for a given account — never Kimi Code's own device-id
/// file, which shunt does not read.
pub(crate) fn msh_headers(device_id: &str) -> [(&'static str, String); 5] {
    [
        ("x-msh-platform", "shunt".to_string()),
        ("x-msh-version", env!("CARGO_PKG_VERSION").to_string()),
        ("x-msh-device-name", device_name()),
        ("x-msh-device-model", device_model()),
        ("x-msh-device-id", device_id.to_string()),
    ]
}

fn device_name() -> String {
    unix_hostname().unwrap_or_else(|| "unknown".to_string())
}

#[cfg(unix)]
fn unix_hostname() -> Option<String> {
    // SAFETY: `buf` is a valid, appropriately-sized writable buffer for the
    // duration of the call; `gethostname` writes at most `buf.len()` bytes
    // (including a NUL terminator) and returns non-zero on failure without
    // touching `buf`.
    let mut buf = vec![0u8; 256];
    let ret = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if ret != 0 {
        return None;
    }
    let len = buf.iter().position(|&byte| byte == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..len]).into_owned();
    (!name.is_empty()).then_some(name)
}

#[cfg(not(unix))]
fn unix_hostname() -> Option<String> {
    None
}

fn device_model() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shunt-kimi-auth-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        dir.join("kimi-auth.json")
    }

    fn write_test_account(
        path: &Path,
        access_token: &str,
        refresh_token: &str,
        expires_at_ms: i64,
        device_id: &str,
    ) {
        // `write_auth_file_atomic` assumes its parent directory already
        // exists (true in production: it's only ever called after a login
        // has created the account directory via `store_oauth_tokens`), so
        // the fixture must create it itself.
        std::fs::create_dir_all(path.parent().expect("account path has a parent")).unwrap();
        let value = json!({
            "kimiOauth": {
                "accessToken": access_token,
                "refreshToken": refresh_token,
                "expiresAt": expires_at_ms
            },
            "deviceId": device_id
        });
        write_auth_file_atomic(path, &value).unwrap();
    }

    #[test]
    fn parses_token_endpoint_response() {
        let value = json!({
            "access_token": "new-access",
            "refresh_token": "new-refresh",
            "token_type": "Bearer",
            "expires_in": 900,
            "scope": "coding"
        });
        let parsed = parse_token_response(&value).unwrap();
        assert_eq!(parsed.access_token, "new-access");
        assert_eq!(parsed.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(parsed.expires_in, Some(900));

        // A response without an access_token is not a valid success.
        assert!(parse_token_response(&json!({"refresh_token": "r"})).is_none());
        // An empty access_token is likewise not a valid success.
        assert!(parse_token_response(&json!({"access_token": ""})).is_none());
    }

    #[test]
    fn refresh_error_classifies_invalid_grant_distinctly_from_bare_status() {
        // invalid_grant on refresh must point at re-running login by name, not
        // just report a status code (the CLIProxyAPI bug this store must not
        // reproduce).
        let invalid_grant = json!({
            "error": "invalid_grant",
            "error_description": "The provided authorization grant is invalid"
        });
        let error = refresh_error(&invalid_grant, reqwest::StatusCode::BAD_REQUEST);
        let AdapterError { response, .. } = error;
        let body = response.into_body();
        drop(body); // Response body isn't inspected here; message routing is
                    // covered by the ShuntError -> response contract elsewhere.
                    // The classification itself is asserted via refresh_tokens
                    // integration tests below (message text in the account
                    // file / task error path).

        // Any other named error includes its description.
        let other = refresh_error(
            &json!({"error": "server_error", "error_description": "boom"}),
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
        );
        assert_eq!(other.message, "authentication failed");

        // A body with no parseable `error` field falls back to the bare status.
        let no_error = refresh_error(&json!({"foo": "bar"}), reqwest::StatusCode::BAD_GATEWAY);
        assert_eq!(no_error.message, "authentication failed");
    }

    #[test]
    fn expires_at_ms_uses_expires_in_and_falls_back() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        assert_eq!(expires_at_ms(Some(120), now), 1_000_000 + 120_000);
        assert_eq!(
            expires_at_ms(None, now),
            1_000_000 + DEFAULT_EXPIRES_IN_SECS * 1000
        );
    }

    #[test]
    fn write_back_round_trips_and_preserves_device_id() {
        let path = temp_path("writeback");
        write_test_account(&path, "access-1", "refresh-1", 0, "device-abc");

        write_back(&path, "access-2", "refresh-2", 4_000_000_000_000).unwrap();

        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["kimiOauth"]["accessToken"], "access-2");
        assert_eq!(value["kimiOauth"]["refreshToken"], "refresh-2");
        assert_eq!(value["kimiOauth"]["expiresAt"], 4_000_000_000_000_i64);
        // The device id (a sibling top-level field, untouched by write_back)
        // survives the refresh writeback.
        assert_eq!(value["deviceId"], "device-abc");

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn written_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_path("perms");
        write_test_account(&path, "a", "r", 0, "device-1");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn msh_headers_carry_platform_version_and_device_id() {
        let headers = msh_headers("device-xyz");
        let map: std::collections::HashMap<_, _> = headers.into_iter().collect();
        assert_eq!(map["x-msh-platform"], "shunt");
        assert_eq!(map["x-msh-version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(map["x-msh-device-id"], "device-xyz");
        assert!(!map["x-msh-device-name"].is_empty());
        assert!(!map["x-msh-device-model"].is_empty());
    }

    #[tokio::test]
    async fn refresh_pending_style_400_is_parsed_and_classified_not_gated_on_status() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Regression for the CRITICAL fact: Kimi's token endpoint returns HTTP
        // 400 with a JSON `invalid_grant` body for a dead refresh token. A
        // status-first implementation (xAI's `refresh_tokens` pattern) would
        // report a bare "HTTP 400" instead of the specific, actionable
        // invalid_grant classification.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "invalid_grant",
                "error_description": "The provided authorization grant is invalid"
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let token_url = format!("{}/token", server.uri());
        let error = refresh_tokens(&client, &token_url, "dead-refresh", "device-1")
            .await
            .unwrap_err();
        assert_eq!(error.message, "authentication failed");
        // The body was parsed and classified, not just relayed as a status —
        // exercised via get_valid below where the message reaches the caller.
    }

    #[tokio::test]
    async fn get_valid_returns_the_stored_token_when_not_expired() {
        let path = temp_path("valid");
        let far_future = expires_at_ms(Some(3600), SystemTime::now());
        write_test_account(&path, "still-good", "refresh", far_future, "device-1");

        let store = KimiAuthStore::new(path.clone(), reqwest::Client::new());
        let cred = store.get_valid().await.unwrap();
        assert_eq!(cred.access_token, "still-good");
        assert_eq!(cred.device_id.as_deref(), Some("device-1"));

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn get_valid_refreshes_and_writes_back_preserving_device_id() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "refreshed-access",
                "refresh_token": "refreshed-refresh",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        let path = temp_path("refresh");
        write_test_account(&path, "stale-access", "old-refresh", 0, "device-42");
        let store = KimiAuthStore::with_token_url(
            path.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
        );

        let cred = store.get_valid().await.unwrap();
        assert_eq!(cred.access_token, "refreshed-access");
        assert_eq!(cred.device_id.as_deref(), Some("device-42"));

        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["kimiOauth"]["accessToken"], "refreshed-access");
        assert_eq!(value["kimiOauth"]["refreshToken"], "refreshed-refresh");
        assert_eq!(value["deviceId"], "device-42");

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn refresh_omitting_refresh_token_is_reused_leniently() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Kimi's refresh-rotation behavior is unmeasured; the store follows
        // Claude's lenient policy (reuse the existing refresh token when the
        // response omits one) rather than xAI's strict always-rotates policy.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "refreshed-access",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        let path = temp_path("lenient");
        write_test_account(&path, "stale-access", "kept-refresh", 0, "device-9");
        let store = KimiAuthStore::with_token_url(
            path.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
        );

        store.get_valid().await.unwrap();
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["kimiOauth"]["refreshToken"], "kept-refresh");

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn refresh_invalid_grant_is_terminal_and_mentions_relogin_by_name() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "invalid_grant",
                "error_description": "The provided authorization grant is invalid"
            })))
            .mount(&server)
            .await;

        let path = temp_path("invalidgrant");
        write_test_account(&path, "stale-access", "dead-refresh", 0, "device-1");
        let store = KimiAuthStore::with_token_url(
            path.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
        );

        let error = store.get_valid().await.unwrap_err();
        assert_eq!(error.message, "authentication failed");
        // The stored (dead) pair is untouched — nothing was persisted on a
        // terminal failure.
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["kimiOauth"]["refreshToken"], "dead-refresh");

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn concurrent_get_valid_single_flights_the_refresh() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "refreshed-access",
                "refresh_token": "rotated-refresh",
                "expires_in": 3600
            })))
            .expect(1)
            .mount(&server)
            .await;

        let path = temp_path("singleflight");
        write_test_account(&path, "stale-access", "old-refresh", 0, "device-1");
        let store = KimiAuthStore::with_token_url(
            path.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
        );

        let (first, second) = tokio::join!(store.get_valid(), store.get_valid());
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(first, second);
        assert_eq!(first.access_token, "refreshed-access");

        server.verify().await;
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
