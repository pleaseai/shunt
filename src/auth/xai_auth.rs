//! xAI subscription OAuth credential store.
//!
//! Mirrors [`super::codex_auth`]: read the shunt-owned credential file fresh on
//! every call, decide expiry from the access token's JWT `exp` (5-minute
//! buffer), refresh against xAI's token endpoint when stale, and write the
//! rotated pair back atomically at `0600`. Token values are never logged — only
//! refresh outcomes. The credential file (`~/.shunt/xai-auth.json`, overridable
//! via `SHUNT_XAI_AUTH_FILE`) is written by `shunt login xai` (see
//! [`super::xai_login`]) and owned solely by shunt.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde_json::{json, Value};

use crate::adapters::AdapterError;
use crate::auth::auth_error;
use crate::auth::shared::{format_iso8601, is_token_valid_at, write_auth_file_atomic};

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
}

/// In-process single-flight for the refresh path. xAI rotates the refresh
/// token on every refresh, so of two concurrent refreshes the loser would
/// replay an already-consumed token and fail. The winner refreshes under this
/// lock; waiters re-read the file and find the fresh pair. Cross-process races
/// are out of scope — shunt owns the file and one gateway process is the norm.
static REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

    /// Return a valid access token, refreshing (and persisting the rotated
    /// refresh token) when the stored one is within the 5-minute expiry buffer.
    pub async fn get_valid(&self) -> Result<XaiCred, AdapterError> {
        let tokens = self.read_tokens_off_thread().await?;
        if is_token_valid_at(&tokens.access_token, SystemTime::now()) {
            return Ok(XaiCred {
                access_token: tokens.access_token,
            });
        }

        // Single-flight the refresh: a concurrent caller that already holds the
        // lock is refreshing right now, so once we acquire it, re-read — the
        // rotated pair it wrote is what we must use, not our stale one.
        let refreshing = REFRESH_LOCK.lock().await;
        let tokens = self.read_tokens_off_thread().await?;
        if is_token_valid_at(&tokens.access_token, SystemTime::now()) {
            return Ok(XaiCred {
                access_token: tokens.access_token,
            });
        }

        // Run the refresh + rotated-pair write in a detached task that owns the
        // single-flight guard. `tokio::spawn` keeps running when its JoinHandle
        // is dropped, so a cancelled request (client disconnect) can no longer
        // release the lock mid-write and let another caller re-refresh with the
        // already-consumed refresh token (token-replay race). The lock is held
        // until the write completes, regardless of caller cancellation.
        let client = self.client.clone();
        let token_url = self.token_url.clone();
        let path = self.path.clone();
        let handle = tokio::spawn(async move {
            let _refreshing = refreshing; // held until the write below completes
            let refreshed = refresh_tokens(&client, &token_url, &tokens.refresh_token).await?;
            // xAI rotates the refresh token on every refresh and consumes the old
            // one. A success that omits refresh_token would leave the consumed
            // token on disk and break the next refresh, so treat it as an invalid
            // response instead of persisting a broken pair.
            let Some(refresh_token) = refreshed.refresh_token.as_deref() else {
                return Err(auth_error(
                    "xAI refresh response missing refresh_token; run shunt login xai",
                ));
            };
            let id_token = refreshed.id_token.as_deref().or(tokens.id_token.as_deref());
            write_tokens_off_thread(
                path,
                refreshed.access_token.clone(),
                refresh_token.to_string(),
                id_token.map(ToOwned::to_owned),
            )
            .await
            .map_err(|error| auth_error(format!("failed to update xAI auth file: {error}")))?;
            tracing::info!("refreshed xAI OAuth access token");
            Ok::<String, AdapterError>(refreshed.access_token)
        });
        let access_token = handle
            .await
            .map_err(|error| auth_error(format!("xAI refresh task failed: {error}")))??;
        Ok(XaiCred { access_token })
    }

    fn read_tokens(&self) -> Result<TokenSet, AdapterError> {
        let value = read_auth_value(&self.path)
            .map_err(|error| auth_error(read_error_message(&self.path, &error)))?;
        parse_tokens(&value)
            .ok_or_else(|| auth_error("xAI auth tokens missing; run shunt login xai"))
    }

    /// Read + parse the credential file on the blocking thread pool so the
    /// synchronous file I/O never stalls the async runtime.
    async fn read_tokens_off_thread(&self) -> Result<TokenSet, AdapterError> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.read_tokens())
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
fn read_error_message(path: &Path, error: &io::Error) -> String {
    if error.kind() == io::ErrorKind::NotFound {
        "xAI auth not found; run shunt login xai".to_string()
    } else {
        format!(
            "xAI auth file {} unreadable: {error}; fix the file or run shunt login xai",
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
    let mut tokens = serde_json::Map::new();
    tokens.insert("access_token".to_string(), json!(access_token));
    tokens.insert("refresh_token".to_string(), json!(refresh_token));
    if let Some(id_token) = id_token {
        tokens.insert("id_token".to_string(), json!(id_token));
    }
    let value = json!({
        "tokens": Value::Object(tokens),
        "last_refresh": format_iso8601(SystemTime::now()),
    });
    write_auth_file_atomic(path, &value)
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

pub(crate) async fn refresh_tokens(
    client: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
) -> Result<TokenResponse, AdapterError> {
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
        .map_err(|_| auth_error("failed to refresh xAI auth; run shunt login xai"))?;
    let status = response.status();
    if !status.is_success() {
        tracing::warn!(%status, "xAI token refresh failed");
        return Err(refresh_status_error(status));
    }
    let text = response
        .text()
        .await
        .map_err(|_| auth_error("invalid xAI refresh response; run shunt login xai"))?;
    let value = serde_json::from_str::<Value>(&text)
        .map_err(|_| auth_error("invalid xAI refresh response; run shunt login xai"))?;
    parse_token_response(&value)
        .ok_or_else(|| auth_error("invalid xAI refresh response; run shunt login xai"))
}

pub(crate) fn refresh_status_error(status: reqwest::StatusCode) -> AdapterError {
    auth_error(refresh_error_message(status))
}

/// Map a non-2xx refresh status to a distinct client-facing message. A 403 is a
/// subscription-tier/entitlement gate (re-login won't help — point at the
/// API-key path); 400/401 is a consumed/invalid grant (re-login).
pub(crate) fn refresh_error_message(status: reqwest::StatusCode) -> String {
    if status == reqwest::StatusCode::FORBIDDEN {
        "xAI OAuth account is not authorized for API access (subscription tier gate); \
         re-logging in will not help — set XAI_API_KEY and use the api-key path, \
         or upgrade your subscription at https://x.ai/grok"
            .to_string()
    } else if status == reqwest::StatusCode::BAD_REQUEST
        || status == reqwest::StatusCode::UNAUTHORIZED
    {
        "xAI token refresh rejected (invalid_grant); run shunt login xai".to_string()
    } else {
        format!("xAI token refresh failed (HTTP {status}); run shunt login xai")
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
            read_error_message(Path::new("/tmp/x.json"), &missing),
            "xAI auth not found; run shunt login xai"
        );

        // EACCES / corrupt JSON name the real cause instead of claiming the
        // file doesn't exist and misdirecting the operator into a re-login.
        let denied = io::Error::new(io::ErrorKind::PermissionDenied, "permission denied");
        let message = read_error_message(Path::new("/tmp/x.json"), &denied);
        assert!(message.contains("/tmp/x.json"));
        assert!(message.contains("permission denied"));
        assert!(!message.contains("not found"));
    }

    #[test]
    fn refresh_status_errors_are_distinct() {
        // 403 = tier gate: must NOT tell the user to re-login; points at the key.
        let tier = refresh_error_message(reqwest::StatusCode::FORBIDDEN);
        assert!(tier.contains("tier gate"));
        assert!(tier.contains("XAI_API_KEY"));
        assert!(!tier.contains("run shunt login xai"));

        // 400/401 = invalid grant: tell the user to log in again.
        assert!(
            refresh_error_message(reqwest::StatusCode::BAD_REQUEST).contains("run shunt login xai")
        );
        assert!(refresh_error_message(reqwest::StatusCode::UNAUTHORIZED)
            .contains("run shunt login xai"));
    }

    #[tokio::test]
    async fn refresh_rotates_and_get_valid_writes_back() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": jwt(4_000_000_000),
                "refresh_token": "rotated-refresh",
                "id_token": "rotated-id",
                "expires_in": 900
            })))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let token_url = format!("{}/token", server.uri());
        let refreshed = refresh_tokens(&client, &token_url, "old-refresh")
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
                "access_token": jwt(4_000_000_000),
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
        assert!(refresh_tokens(&client, &token_url, "r").await.is_err());
    }
}
