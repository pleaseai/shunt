use std::{fs, io, path::PathBuf, time::SystemTime};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    adapters::AdapterError,
    auth::{auth_error, codex_auth::write_auth_file_atomic},
};

const EXPIRY_SKEW_SECONDS: u64 = 60;

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
        if token_is_valid(&stored.access_token, SystemTime::now()) {
            return Ok(CursorCred {
                access_token: stored.access_token,
            });
        }
        let _guard = REFRESH_LOCK.lock().await;
        let stored = self.read_off_thread().await?;
        if token_is_valid(&stored.access_token, SystemTime::now()) {
            return Ok(CursorCred {
                access_token: stored.access_token,
            });
        }
        let refresh_token = stored
            .refresh_token
            .as_deref()
            .ok_or_else(|| auth_error("Cursor access token expired; run shunt login cursor"))?;
        let refreshed = refresh(&self.client, &self.base_url, refresh_token).await?;
        write_auth_off_thread(self.path.clone(), refreshed.clone())
            .await
            .map_err(|error| auth_error(format!("failed to update Cursor auth file: {error}")))?;
        tracing::info!("refreshed Cursor OAuth access token");
        Ok(CursorCred {
            access_token: refreshed.access_token,
        })
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
    Some(StoredCursorAuth {
        access_token: value.get("accessToken")?.as_str()?.to_string(),
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
    std::env::var("SHUNT_CURSOR_AUTH_TOKEN")
        .ok()
        .filter(|token| !token.trim().is_empty())
        .or_else(|| {
            std::env::var("CURSOR_AUTH_TOKEN")
                .ok()
                .filter(|token| !token.trim().is_empty())
        })
}

fn token_is_valid(token: &str, now: SystemTime) -> bool {
    let Some(exp) = jwt_claims(token).and_then(|claims| claims.get("exp").and_then(Value::as_u64))
    else {
        return true;
    };
    let now = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    exp > now.saturating_add(EXPIRY_SKEW_SECONDS)
}

pub(crate) fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
