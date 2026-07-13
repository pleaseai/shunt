use std::{fs, io, path::PathBuf, time::SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    adapters::AdapterError,
    auth::{
        auth_error,
        shared::{is_token_valid_at, write_auth_file_atomic},
    },
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredCursorAuth {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorCred {
    pub access_token: String,
}

#[derive(Debug, Clone)]
pub struct CursorAuthStore {
    path: PathBuf,
    client: reqwest::Client,
    base_url: String,
}

static REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

impl CursorAuthStore {
    pub fn new(path: PathBuf, client: reqwest::Client, base_url: impl Into<String>) -> Self {
        Self {
            path,
            client,
            base_url: base_url.into(),
        }
    }

    pub async fn get_valid(&self) -> Result<CursorCred, AdapterError> {
        if let Some(token) = env_token() {
            return Ok(CursorCred {
                access_token: token,
            });
        }
        let stored = self.read_off_thread().await?;
        if is_token_valid_at(&stored.access_token, SystemTime::now()) {
            return Ok(CursorCred {
                access_token: stored.access_token,
            });
        }
        let refreshing = REFRESH_LOCK.lock().await;
        let stored = self.read_off_thread().await?;
        if is_token_valid_at(&stored.access_token, SystemTime::now()) {
            return Ok(CursorCred {
                access_token: stored.access_token,
            });
        }
        let refresh_token = stored
            .refresh_token
            .as_deref()
            .ok_or_else(|| auth_error("Cursor access token expired; run shunt login cursor"))?
            .to_string();

        // Run the refresh + writeback in a detached task that owns the lock, so a
        // cancelled request (e.g. client disconnect) cannot abort the critical
        // section mid-flight — which would release the lock early into a writeback
        // race and, if Cursor ever consumes the old refresh token server-side,
        // strand a spent token in the file. Mirrors the cancellation-safe pattern
        // in `xai_auth`.
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let path = self.path.clone();
        let handle = tokio::spawn(async move {
            let _refreshing = refreshing; // hold the lock until the write completes
            let mut refreshed = refresh(&client, &base_url, &refresh_token).await?;
            // Per RFC 6749 §6, a refresh response MAY omit a new refresh token, in
            // which case the existing one stays valid and must be retained. Cursor
            // is not known to rotate+consume its refresh tokens (unlike xAI), so
            // dropping the old token here would force an avoidable re-login on the
            // next expiry. Preserve it when the response carries no replacement.
            if refreshed.refresh_token.is_none() {
                refreshed.refresh_token = Some(refresh_token);
            }
            write_auth_off_thread(path, refreshed.clone())
                .await
                .map_err(|error| {
                    auth_error(format!("failed to update Cursor auth file: {error}"))
                })?;
            tracing::info!("refreshed Cursor OAuth access token");
            Ok::<String, AdapterError>(refreshed.access_token)
        });
        let access_token = handle
            .await
            .map_err(|error| auth_error(format!("Cursor refresh task failed: {error}")))??;
        Ok(CursorCred { access_token })
    }

    /// Read the credential file on the blocking thread pool so the synchronous
    /// file I/O never stalls the async runtime.
    async fn read_off_thread(&self) -> Result<StoredCursorAuth, AdapterError> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.read())
            .await
            .map_err(|error| auth_error(format!("Cursor auth read task failed: {error}")))?
    }

    fn read(&self) -> Result<StoredCursorAuth, AdapterError> {
        let bytes = fs::read(&self.path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                auth_error("Cursor auth not found; run shunt login cursor")
            } else {
                auth_error(format!(
                    "Cursor auth file {} unreadable: {error}",
                    self.path.display()
                ))
            }
        })?;
        serde_json::from_slice(&bytes)
            .map_err(|error| auth_error(format!("invalid Cursor auth file: {error}")))
    }
}

pub(crate) fn write_auth(path: &std::path::Path, auth: &StoredCursorAuth) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let value = serde_json::to_value(auth)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    write_auth_file_atomic(path, &value)
}

/// Persist the refreshed credential on the blocking thread pool. Same content,
/// path, and atomicity as [`write_auth`] — only the executing thread differs.
async fn write_auth_off_thread(path: PathBuf, auth: StoredCursorAuth) -> io::Result<()> {
    tokio::task::spawn_blocking(move || write_auth(&path, &auth))
        .await
        .map_err(|error| io::Error::other(format!("Cursor auth write task failed: {error}")))?
}

async fn refresh(
    client: &reqwest::Client,
    base_url: &str,
    refresh_token: &str,
) -> Result<StoredCursorAuth, AdapterError> {
    let response = client
        .post(format!("{}/auth/refresh", base_url.trim_end_matches('/')))
        .bearer_auth(refresh_token)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .map_err(|_| auth_error("failed to refresh Cursor auth; run shunt login cursor"))?;
    if !response.status().is_success() {
        return Err(auth_error(format!(
            "Cursor token refresh failed (HTTP {}); run shunt login cursor",
            response.status()
        )));
    }
    let text = response
        .text()
        .await
        .map_err(|_| auth_error("invalid Cursor refresh response; run shunt login cursor"))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| auth_error("invalid Cursor refresh response; run shunt login cursor"))?;
    parse_token_response(&value)
        .ok_or_else(|| auth_error("invalid Cursor refresh response; run shunt login cursor"))
}

pub(crate) fn parse_token_response(value: &Value) -> Option<StoredCursorAuth> {
    // An empty accessToken is not a usable credential; treat a malformed success
    // response as invalid rather than persisting a broken token that then fails
    // every request.
    let access_token = value.get("accessToken")?.as_str()?;
    if access_token.is_empty() {
        return None;
    }
    Some(StoredCursorAuth {
        access_token: access_token.to_string(),
        refresh_token: value
            .get("refreshToken")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        api_key: value
            .get("apiKey")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    })
}

fn env_token() -> Option<String> {
    // Resolve once process-wide: `get_valid` runs per request and `std::env::var`
    // takes a global lock, so cache the lookup (the override is deploy-time
    // config, not per-request).
    static TOKEN: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    TOKEN
        .get_or_init(|| {
            if let Some(token) = std::env::var("SHUNT_CURSOR_AUTH_TOKEN")
                .ok()
                .filter(|token| !token.trim().is_empty())
            {
                return Some(token);
            }
            // Fall back to the un-namespaced `CURSOR_AUTH_TOKEN`. The Cursor desktop
            // app uses the same variable name, so a developer's shell may have it
            // set for the IDE; consuming it here silently bypasses the stored
            // `cursor-auth.json` and its refresh logic. Warn so an unexpected 401
            // (from an expired IDE token) is traceable to the env var.
            let token = std::env::var("CURSOR_AUTH_TOKEN")
                .ok()
                .filter(|token| !token.trim().is_empty())?;
            tracing::warn!(
                "using CURSOR_AUTH_TOKEN from the environment (not SHUNT_CURSOR_AUTH_TOKEN); \
                 this bypasses the stored Cursor credential file and its token refresh"
            );
            Some(token)
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::shared::jwt_claims;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use serde_json::json;

    #[test]
    fn parses_camel_case_tokens() {
        let auth = parse_token_response(&json!({
            "accessToken": "access",
            "refreshToken": "refresh",
            "apiKey": "key"
        }))
        .unwrap();
        assert_eq!(auth.access_token, "access");
        assert_eq!(auth.refresh_token.as_deref(), Some("refresh"));
    }

    #[test]
    fn parses_jwt_claims() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"exp":4102444800,"sub":"user"}"#);
        let claims = jwt_claims(&format!("x.{payload}.y")).unwrap();
        assert_eq!(claims["sub"], "user");
    }

    #[test]
    fn token_without_parseable_exp_is_invalid() {
        // Fail-closed: a token we cannot read an exp from is treated as invalid
        // so it refreshes instead of failing upstream with a 401.
        assert!(!is_token_valid_at("not-a-jwt", SystemTime::now()));
        let no_exp = format!("x.{}.y", URL_SAFE_NO_PAD.encode(br#"{"sub":"user"}"#));
        assert!(!is_token_valid_at(&no_exp, SystemTime::now()));
    }

    fn jwt(exp: u64) -> String {
        let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("x.{payload}.y")
    }

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "shunt-cursor-auth-{tag}-{}",
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
            .join("cursor-auth.json")
    }

    #[tokio::test]
    async fn refresh_response_without_token_preserves_existing_refresh_token() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Cursor's refresh may omit refreshToken; the existing one stays valid
        // (RFC 6749 §6) and must survive the writeback so the next refresh works.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "accessToken": jwt(4_000_000_000)
            })))
            .mount(&server)
            .await;

        let file = temp_path("preserve");
        write_auth(
            &file,
            &StoredCursorAuth {
                access_token: jwt(0),
                refresh_token: Some("old-refresh".to_string()),
                api_key: None,
            },
        )
        .unwrap();

        let store = CursorAuthStore::new(file.clone(), reqwest::Client::new(), server.uri());
        let cred = store.get_valid().await.unwrap();
        assert!(is_token_valid_at(&cred.access_token, SystemTime::now()));

        // The old refresh token was retained, not dropped.
        let stored: StoredCursorAuth = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(stored.refresh_token.as_deref(), Some("old-refresh"));
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[tokio::test]
    async fn refresh_response_with_token_rotates_it() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // When Cursor does return a new refreshToken, it replaces the old one.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/auth/refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "accessToken": jwt(4_000_000_000),
                "refreshToken": "new-refresh"
            })))
            .mount(&server)
            .await;

        let file = temp_path("rotate");
        write_auth(
            &file,
            &StoredCursorAuth {
                access_token: jwt(0),
                refresh_token: Some("old-refresh".to_string()),
                api_key: None,
            },
        )
        .unwrap();

        let store = CursorAuthStore::new(file.clone(), reqwest::Client::new(), server.uri());
        store.get_valid().await.unwrap();

        let stored: StoredCursorAuth = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        assert_eq!(stored.refresh_token.as_deref(), Some("new-refresh"));
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }
}
