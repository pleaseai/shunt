//! Google OAuth subscription credential store (Gemini Code Assist / Google One AI Pro).
//!
//! Reuses the Google OAuth token (`~/.gemini/oauth_creds.json`), optionally
//! refreshes it with operator-supplied client credentials, discovers and caches
//! [`GoogleCred`] containing the `access_token` and `project_id`.

use std::{
    fs, io,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::RwLock;

use crate::adapters::AdapterError;
use crate::auth::auth_error;
use crate::auth::shared::{is_token_valid_at, write_auth_file_atomic};

const CLIENT_ID_ENV: &str = "SHUNT_GOOGLE_CLIENT_ID";
const CLIENT_SECRET_ENV: &str = "SHUNT_GOOGLE_CLIENT_SECRET";
pub(crate) const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub(crate) const LOAD_CODE_ASSIST_URL: &str =
    "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";

/// Expiry buffer: refresh 5 minutes before expiry_date.
const EXPIRY_BUFFER: Duration = Duration::from_secs(5 * 60);

/// In-process single-flight for token refresh across concurrent requests.
static REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleCred {
    pub access_token: String,
    pub project_id: String,
}

#[derive(Debug, Clone)]
pub struct GoogleAuthStore {
    path: PathBuf,
    client: reqwest::Client,
    token_url: String,
    load_code_assist_url: String,
    refresh_client_credentials: Option<(String, String)>,
    project_cache: Arc<RwLock<Option<String>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OAuthCredsFile {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub expiry_date: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TokenRefreshResponse {
    pub access_token: String,
    pub expires_in: Option<u64>,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub scope: Option<String>,
    pub token_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LoadCodeAssistResponse {
    #[serde(alias = "cloudaicompanionProject", alias = "companionProject")]
    pub cloudaicompanion_project: Option<String>,
    #[serde(alias = "projectId")]
    pub project_id: Option<String>,
}

impl GoogleAuthStore {
    pub fn new(path: PathBuf, client: reqwest::Client) -> Self {
        Self {
            path,
            client,
            token_url: TOKEN_URL.to_string(),
            load_code_assist_url: LOAD_CODE_ASSIST_URL.to_string(),
            refresh_client_credentials: None,
            project_cache: Arc::new(RwLock::new(None)),
        }
    }

    #[cfg(test)]
    pub fn with_urls(
        path: PathBuf,
        client: reqwest::Client,
        token_url: String,
        load_code_assist_url: String,
    ) -> Self {
        Self {
            path,
            client,
            token_url,
            load_code_assist_url,
            refresh_client_credentials: Some((
                "test-client-id".to_string(),
                "test-client-secret".to_string(),
            )),
            project_cache: Arc::new(RwLock::new(None)),
        }
    }

    /// Read and return a valid access token + project ID, refreshing when stale.
    pub async fn get_valid(&self) -> Result<GoogleCred, AdapterError> {
        let creds = self.read_creds_with_retry().await?;
        let now = SystemTime::now();

        if is_creds_valid(&creds, now) {
            let project_id = self.get_or_load_project(&creds.access_token).await?;
            return Ok(GoogleCred {
                access_token: creds.access_token,
                project_id,
            });
        }

        // Single-flight the refresh call
        let _guard = REFRESH_LOCK.lock().await;

        // Re-read in case another thread refreshed while we waited
        if let Ok(re_read) = self.read_creds().await {
            if is_creds_valid(&re_read, now) {
                let project_id = self.get_or_load_project(&re_read.access_token).await?;
                return Ok(GoogleCred {
                    access_token: re_read.access_token,
                    project_id,
                });
            }
        }

        // Perform HTTP refresh
        let refreshed = self.refresh_token_call(&creds.refresh_token).await?;

        // Calculate new expiry_date (ms since epoch)
        let expires_in_sec = refreshed.expires_in.unwrap_or(3600);
        let now_ms = now
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let new_expiry_date = now_ms + (expires_in_sec * 1000);

        let new_refresh = refreshed
            .refresh_token
            .unwrap_or_else(|| creds.refresh_token.clone());

        let updated_file = OAuthCredsFile {
            access_token: refreshed.access_token.clone(),
            refresh_token: new_refresh,
            scope: refreshed.scope.or(creds.scope),
            token_type: refreshed.token_type.or(creds.token_type),
            id_token: refreshed.id_token.or(creds.id_token),
            expiry_date: Some(new_expiry_date),
        };

        if let Ok(val) = serde_json::to_value(&updated_file) {
            let _ = write_auth_file_atomic(&self.path, &val);
        }

        let project_id = self.get_or_load_project(&refreshed.access_token).await?;
        Ok(GoogleCred {
            access_token: refreshed.access_token,
            project_id,
        })
    }

    async fn read_creds_with_retry(&self) -> Result<OAuthCredsFile, AdapterError> {
        match self.read_creds().await {
            Ok(creds) => Ok(creds),
            Err(_) => {
                // Retry once after 50ms to tolerate atomic write (delete->rename) races
                tokio::time::sleep(Duration::from_millis(50)).await;
                self.read_creds().await
            }
        }
    }

    async fn read_creds(&self) -> Result<OAuthCredsFile, AdapterError> {
        let path = self.path.clone();
        let content = tokio::task::spawn_blocking(move || fs::read_to_string(&path))
            .await
            .map_err(|_| auth_error("failed to read Google credentials"))?
            .map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    auth_error(format!(
                        "Google OAuth credential file not found at {}. Please run `gemini login` to generate credentials.",
                        self.path.display()
                    ))
                } else {
                    auth_error(format!(
                        "failed to read Google OAuth credential file {}: {error}",
                        self.path.display()
                    ))
                }
            })?;

        serde_json::from_str::<OAuthCredsFile>(&content).map_err(|error| {
            auth_error(format!(
                "invalid JSON in Google OAuth credential file {}: {error}",
                self.path.display()
            ))
        })
    }

    async fn refresh_token_call(
        &self,
        refresh_token: &str,
    ) -> Result<TokenRefreshResponse, AdapterError> {
        let (client_id, client_secret) = match &self.refresh_client_credentials {
            Some(credentials) => credentials.clone(),
            None => {
                let client_id = std::env::var(CLIENT_ID_ENV).map_err(|_| {
                    auth_error(format!(
                        "Google OAuth token needs refresh; set {CLIENT_ID_ENV} and {CLIENT_SECRET_ENV}, or run `gemini login` to refresh the shared credential file"
                    ))
                })?;
                let client_secret = std::env::var(CLIENT_SECRET_ENV).map_err(|_| {
                    auth_error(format!(
                        "Google OAuth token needs refresh; set {CLIENT_ID_ENV} and {CLIENT_SECRET_ENV}, or run `gemini login` to refresh the shared credential file"
                    ))
                })?;
                (client_id, client_secret)
            }
        };
        let params = [
            ("grant_type", "refresh_token"),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("refresh_token", refresh_token),
        ];

        let response = self
            .client
            .post(&self.token_url)
            .form(&params)
            .send()
            .await
            .map_err(|error| {
                auth_error(format!("Google token refresh network failure: {error}"))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            tracing::warn!(
                status = %status,
                body = %body,
                "Google OAuth token refresh rejected"
            );
            return Err(auth_error(
                "Google OAuth token expired or revoked. Please run `gemini login` to refresh credentials.",
            ));
        }

        response
            .json::<TokenRefreshResponse>()
            .await
            .map_err(|error| {
                auth_error(format!(
                    "invalid JSON response from Google token endpoint: {error}"
                ))
            })
    }

    async fn get_or_load_project(&self, access_token: &str) -> Result<String, AdapterError> {
        {
            let cache = self.project_cache.read().await;
            if let Some(project) = cache.as_ref() {
                return Ok(project.clone());
            }
        }

        let project = self.load_code_assist(access_token).await?;
        let mut cache = self.project_cache.write().await;
        *cache = Some(project.clone());
        Ok(project)
    }

    async fn load_code_assist(&self, access_token: &str) -> Result<String, AdapterError> {
        let response = self
            .client
            .post(&self.load_code_assist_url)
            .bearer_auth(access_token)
            .json(&json!({}))
            .send()
            .await
            .map_err(|error| {
                auth_error(format!("Code Assist discovery network failure: {error}"))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            tracing::warn!(
                status = %status,
                body = %body,
                "Code Assist discovery loadCodeAssist rejected"
            );
            return Err(auth_error(format!(
                "Code Assist project discovery failed with HTTP status {status}"
            )));
        }

        let body = response
            .json::<LoadCodeAssistResponse>()
            .await
            .map_err(|error| auth_error(format!("invalid JSON from loadCodeAssist: {error}")))?;

        body.cloudaicompanion_project
            .or(body.project_id)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| auth_error("loadCodeAssist response missing cloudaicompanionProject ID"))
    }
}

fn is_creds_valid(creds: &OAuthCredsFile, now: SystemTime) -> bool {
    if let Some(expiry_date_ms) = creds.expiry_date {
        let expiry_time = UNIX_EPOCH + Duration::from_millis(expiry_date_ms);
        if let Some(refresh_at) = expiry_time.checked_sub(EXPIRY_BUFFER) {
            return now < refresh_at;
        }
    }
    is_token_valid_at(&creds.access_token, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{bearer_token, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn temp_auth_file(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("shunt_tests_google_{}_{}", name, id));
        let _ = fs::create_dir_all(&dir);
        dir.join("oauth_creds.json")
    }

    #[tokio::test]
    async fn valid_token_returns_creds_without_refresh() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .and(bearer_token("valid-access-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "cloudaicompanionProject": "test-project-123"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let file = temp_auth_file("valid_token");
        let future_expiry = (SystemTime::now() + Duration::from_secs(3600))
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let creds_json = json!({
            "access_token": "valid-access-token",
            "refresh_token": "valid-refresh-token",
            "expiry_date": future_expiry
        });
        fs::write(&file, serde_json::to_string(&creds_json).unwrap()).unwrap();

        let store = GoogleAuthStore::with_urls(
            file.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
            format!("{}/v1internal:loadCodeAssist", server.uri()),
        );

        let cred = store.get_valid().await.unwrap();
        assert_eq!(cred.access_token, "valid-access-token");
        assert_eq!(cred.project_id, "test-project-123");

        let _ = fs::remove_dir_all(file.parent().unwrap());
    }

    #[tokio::test]
    async fn expired_token_refreshes_and_persists() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "refreshed-access-token",
                "refresh_token": "new-refresh-token",
                "expires_in": 3600
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1internal:loadCodeAssist"))
            .and(bearer_token("refreshed-access-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "cloudaicompanionProject": "refreshed-project-456"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let file = temp_auth_file("expired_token");
        let past_expiry = 1000000000000_u64; // past date

        let creds_json = json!({
            "access_token": "old-access-token",
            "refresh_token": "old-refresh-token",
            "expiry_date": past_expiry
        });
        fs::write(&file, serde_json::to_string(&creds_json).unwrap()).unwrap();

        let store = GoogleAuthStore::with_urls(
            file.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
            format!("{}/v1internal:loadCodeAssist", server.uri()),
        );

        let cred = store.get_valid().await.unwrap();
        assert_eq!(cred.access_token, "refreshed-access-token");
        assert_eq!(cred.project_id, "refreshed-project-456");

        // Verify disk file updated
        let content = fs::read_to_string(&file).unwrap();
        let updated: OAuthCredsFile = serde_json::from_str(&content).unwrap();
        assert_eq!(updated.access_token, "refreshed-access-token");
        assert_eq!(updated.refresh_token, "new-refresh-token");

        let _ = fs::remove_dir_all(file.parent().unwrap());
    }

    #[tokio::test]
    async fn revoked_refresh_token_returns_clear_auth_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error": "invalid_grant"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let file = temp_auth_file("revoked_token");
        let creds_json = json!({
            "access_token": "old-access-token",
            "refresh_token": "revoked-refresh-token",
            "expiry_date": 1000000000000_u64
        });
        fs::write(&file, serde_json::to_string(&creds_json).unwrap()).unwrap();

        let store = GoogleAuthStore::with_urls(
            file.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
            format!("{}/v1internal:loadCodeAssist", server.uri()),
        );

        let error = store.get_valid().await.unwrap_err();
        assert!(
            error.message.contains("authentication failed")
                || error.message.contains("gemini login")
        );

        let _ = fs::remove_dir_all(file.parent().unwrap());
    }
}
