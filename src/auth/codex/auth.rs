use std::{
    collections::HashMap,
    env, fs, io,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex as StdMutex, Weak},
    time::SystemTime,
};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::adapters::AdapterError;
use crate::auth::auth_error;
use crate::auth::shared::{format_iso8601, is_token_valid_at, jwt_claims, write_auth_file_atomic};

pub(crate) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub(crate) const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatGptCred {
    pub access_token: String,
    pub account_id: String,
}

#[derive(Debug, Clone)]
pub struct CodexAuthStore {
    path: PathBuf,
    client: reqwest::Client,
    token_url: String,
}

/// In-process single-flight registry for ChatGPT OAuth refreshes, keyed by
/// credential path. Stores are constructed per request, so the registry must
/// be shared across independent instances. Keying by path lets refreshes for
/// different accounts (different credential files) proceed in parallel while
/// still serializing concurrent refreshes of the *same* file. Entries are
/// held `Weak`, so a path's lock is dropped once nothing is refreshing it,
/// keeping the registry from growing unbounded over the process lifetime.
/// The refresh task owns the guard through atomic writeback, preventing a
/// cancelled caller from exposing an already-consumed refresh token to
/// another request.
static REFRESH_LOCKS: LazyLock<StdMutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// Return the single-flight lock for `path`, creating and registering one if
/// none exists (or the previous one has been dropped). Opportunistically
/// prunes dead entries so paths that are no longer being refreshed don't
/// linger in the registry forever.
fn refresh_lock_for(path: &Path) -> Arc<tokio::sync::Mutex<()>> {
    let mut locks = REFRESH_LOCKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
    locks.retain(|_, weak| weak.strong_count() > 0);
    lock
}

/// Resolve the Codex OAuth refresh endpoint from a raw `SHUNT_CODEX_TOKEN_URL`
/// value, rejecting an empty, malformed, or non-loopback-plaintext override that
/// would egress the long-lived `refresh_token` off-origin or in the clear. Binds
/// the shared [`crate::auth::shared::sanitize_token_url`] guard to the production
/// `auth.openai.com` default; the Claude store shares the same guard (#118) so
/// the two cannot drift.
pub(crate) fn resolve_oauth_token_url() -> String {
    crate::auth::shared::sanitize_token_url(env::var("SHUNT_CODEX_TOKEN_URL").ok(), TOKEN_URL)
}

impl CodexAuthStore {
    pub fn new(path: PathBuf, client: reqwest::Client) -> Self {
        // `SHUNT_CODEX_TOKEN_URL` overrides the OAuth refresh endpoint, mirroring
        // `ClaudeAuthStore::new`'s `SHUNT_CLAUDE_TOKEN_URL`. It exists purely as a
        // test seam (see `force_refresh_refreshes_a_still_valid_chatgpt_token`
        // below) — left unset, production refreshes against the real
        // `auth.openai.com` endpoint. `resolve_oauth_token_url` rejects a non-loopback
        // plaintext override so a misconfigured env var can never egress the
        // long-lived `refresh_token` off-origin or in the clear.
        let token_url = resolve_oauth_token_url();
        Self {
            path,
            client,
            token_url,
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

    pub async fn get_valid_chatgpt(&self) -> Result<ChatGptCred, AdapterError> {
        let auth = self.read_auth_off_thread().await?;
        let tokens = auth
            .tokens()
            .ok_or_else(|| auth_error("ChatGPT auth tokens missing; run codex login"))?;
        if tokens.is_valid_at(SystemTime::now()) {
            return tokens.to_credential();
        }

        // Single-flight the refresh, scoped to this credential path. A waiter
        // re-reads after acquiring the lock, so it uses the credential
        // persisted by the caller that refreshed first.
        let refreshing = refresh_lock_for(&self.path).lock_owned().await;
        let auth = self.read_auth_off_thread().await?;
        let tokens = auth
            .tokens()
            .ok_or_else(|| auth_error("ChatGPT auth tokens missing; run codex login"))?;
        if tokens.is_valid_at(SystemTime::now()) {
            return tokens.to_credential();
        }

        self.refresh_and_write_back(tokens, refreshing).await
    }

    /// Refresh and persist the stored ChatGPT credential unconditionally,
    /// skipping the local expiry check `get_valid_chatgpt` performs. Used by
    /// the account pool's `RefreshRetry` failover arm after an upstream 401 —
    /// the cached token may still look unexpired locally, but the backend has
    /// already rejected it, so the cache can't be trusted here.
    pub async fn force_refresh(&self) -> Result<ChatGptCred, AdapterError> {
        let refreshing = refresh_lock_for(&self.path).lock_owned().await;
        let auth = self.read_auth_off_thread().await?;
        let tokens = auth
            .tokens()
            .ok_or_else(|| auth_error("ChatGPT auth tokens missing; run codex login"))?;
        self.refresh_and_write_back(tokens, refreshing).await
    }

    async fn refresh_and_write_back(
        &self,
        tokens: TokenSet,
        refreshing: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<ChatGptCred, AdapterError> {
        let refresh_token = tokens
            .refresh_token
            .clone()
            .ok_or_else(|| auth_error("ChatGPT refresh token missing; run codex login"))?;

        // The detached task owns both the single-flight guard and the critical
        // refresh + writeback sequence. Dropping the caller's future therefore
        // cannot strand a rotated refresh token in memory after the provider has
        // consumed the old one.
        let client = self.client.clone();
        let token_url = self.token_url.clone();
        let path = self.path.clone();
        tokio::spawn(async move {
            let _refreshing = refreshing;
            let refreshed = refresh_tokens(&client, &token_url, &refresh_token).await?;
            let result = match refreshed.to_credential() {
                Ok(credential) => {
                    tokio::task::spawn_blocking(move || write_refreshed_auth(&path, refreshed))
                        .await
                        .map_err(|error| {
                            auth_error(format!("ChatGPT auth write task failed: {error}"))
                        })
                        .and_then(|result| {
                            result.map_err(|error| {
                                auth_error(format!(
                                    "failed to update ChatGPT auth file: {error}"
                                ))
                            })
                        })
                        .map(|()| credential)
                }
                Err(error) => Err(error),
            };
            if let Err(error) = &result {
                // The upstream refresh may already have consumed the old refresh
                // token, so a credential-validation or writeback failure can leave
                // the file holding a now-invalid token. Log here (not only via the
                // returned Err) so the failure stays visible even when the caller's
                // future was dropped and the JoinHandle carrying this Err is
                // discarded. Never log the token values themselves, only the error.
                tracing::warn!(
                    ?error,
                    "ChatGPT OAuth token refreshed upstream but validation or writeback failed; stored refresh token may now be stale until re-login"
                );
            }
            result
        })
        .await
        .map_err(|error| auth_error(format!("ChatGPT refresh task failed: {error}")))?
    }

    /// Read the credential file on the blocking thread pool so the synchronous
    /// file I/O never stalls the async runtime.
    async fn read_auth_off_thread(&self) -> Result<AuthFile, AdapterError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || read_auth_file(&path))
            .await
            .map_err(|error| auth_error(format!("ChatGPT auth read task failed: {error}")))?
            .map_err(|_| auth_error("ChatGPT auth not found; run codex login"))
    }
}

#[derive(Debug, Clone)]
pub struct RefreshResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
}

#[derive(Debug, Clone)]
struct AuthFile {
    value: Value,
}

#[derive(Debug, Clone)]
struct TokenSet {
    access_token: String,
    refresh_token: Option<String>,
    account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
}

pub fn read_openai_api_key(path: &Path) -> Option<String> {
    let auth = read_auth_file(path).ok()?;
    if auth.value.get("auth_mode").and_then(Value::as_str) != Some("ApiKey") {
        return None;
    }
    auth.value
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub fn jwt_account_id(token: &str) -> Option<String> {
    jwt_claims(token)?
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub fn parse_refresh_response(value: &Value) -> Option<RefreshResponse> {
    let parsed: OAuthTokenResponse = serde_json::from_value(value.clone()).ok()?;
    Some(RefreshResponse {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        id_token: parsed.id_token,
    })
}

pub fn apply_refresh(value: &mut Value, response: RefreshResponse, now: SystemTime) {
    let account_id = jwt_account_id(&response.access_token);
    let tokens = value
        .as_object_mut()
        .expect("auth file root is an object")
        .entry("tokens")
        .or_insert_with(|| json!({}));
    let tokens = tokens
        .as_object_mut()
        .expect("auth file tokens is an object");
    tokens.insert(
        "access_token".to_string(),
        Value::String(response.access_token.clone()),
    );
    if let Some(refresh_token) = response.refresh_token {
        tokens.insert("refresh_token".to_string(), Value::String(refresh_token));
    }
    if let Some(id_token) = response.id_token {
        tokens.insert("id_token".to_string(), Value::String(id_token));
    }
    if let Some(account_id) = account_id {
        tokens.insert("account_id".to_string(), Value::String(account_id));
    }
    value
        .as_object_mut()
        .expect("auth file root is an object")
        .insert(
            "last_refresh".to_string(),
            Value::String(format_iso8601(now)),
        );
}

async fn refresh_tokens(
    // The injected proxy client follows redirects freely; the refresh POST
    // carries the long-lived refresh_token, so it goes through the
    // redirect-hardened `token_refresh_client()` instead — a permitted token
    // endpoint must not be able to 3xx the credential to a plaintext/off-loopback
    // host and defeat the initial-URL-only `sanitize_token_url` guard.
    _client: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
) -> Result<RefreshResponse, AdapterError> {
    let response = crate::auth::shared::token_refresh_client()
        .post(token_url)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .await
        .map_err(|_| auth_error("failed to refresh ChatGPT auth; run codex login"))?;
    if !response.status().is_success() {
        return Err(auth_error(
            "failed to refresh ChatGPT auth; run codex login",
        ));
    }
    let text = response
        .text()
        .await
        .map_err(|_| auth_error("invalid ChatGPT refresh response; run codex login"))?;
    let value = serde_json::from_str::<Value>(&text)
        .map_err(|_| auth_error("invalid ChatGPT refresh response; run codex login"))?;
    parse_refresh_response(&value)
        .ok_or_else(|| auth_error("invalid ChatGPT refresh response; run codex login"))
}

fn read_auth_file(path: &Path) -> io::Result<AuthFile> {
    let value = serde_json::from_slice(&fs::read(path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(AuthFile { value })
}

fn write_refreshed_auth(path: &Path, response: RefreshResponse) -> io::Result<()> {
    let mut auth = read_auth_file(path)?.value;
    apply_refresh(&mut auth, response, SystemTime::now());
    write_auth_file_atomic(path, &auth)
}

impl AuthFile {
    fn tokens(&self) -> Option<TokenSet> {
        let tokens = self.value.get("tokens")?;
        Some(TokenSet {
            access_token: tokens.get("access_token")?.as_str()?.to_string(),
            refresh_token: tokens
                .get("refresh_token")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            account_id: tokens
                .get("account_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned),
        })
    }
}

impl TokenSet {
    fn is_valid_at(&self, now: SystemTime) -> bool {
        is_token_valid_at(&self.access_token, now)
    }

    fn account_id(&self) -> Option<String> {
        self.account_id
            .clone()
            .or_else(|| jwt_account_id(&self.access_token))
    }

    fn to_credential(&self) -> Result<ChatGptCred, AdapterError> {
        Ok(ChatGptCred {
            access_token: self.access_token.clone(),
            account_id: self
                .account_id()
                .ok_or_else(|| auth_error("ChatGPT account id missing; run codex login"))?,
        })
    }
}

impl RefreshResponse {
    fn to_credential(&self) -> Result<ChatGptCred, AdapterError> {
        Ok(ChatGptCred {
            access_token: self.access_token.clone(),
            account_id: jwt_account_id(&self.access_token)
                .ok_or_else(|| auth_error("ChatGPT account id missing; run codex login"))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Condvar;
    use std::time::{Duration, UNIX_EPOCH};

    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

    use super::*;
    use crate::auth::shared::jwt_exp;

    fn token(exp: u64, account_id: Option<&str>) -> String {
        let payload = if let Some(account_id) = account_id {
            json!({"exp": exp, "https://api.openai.com/auth": {"chatgpt_account_id": account_id}})
        } else {
            json!({"exp": exp})
        };
        format!(
            "x.{}.y",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        )
    }

    fn temp_auth_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "shunt-codex-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn write_auth(path: &Path, access_token: &str, refresh_token: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            json!({
                "auth_mode": "ChatGPT",
                "tokens": {
                    "access_token": access_token,
                    "refresh_token": refresh_token,
                }
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn decodes_jwt_exp_and_account_id_claim() {
        let access = token(2_000_000_000, Some("acct_123"));

        assert_eq!(
            jwt_exp(&access).unwrap(),
            UNIX_EPOCH + Duration::from_secs(2_000_000_000)
        );
        assert_eq!(jwt_account_id(&access).as_deref(), Some("acct_123"));
    }

    #[test]
    fn resolve_oauth_token_url_rejects_off_origin_and_plaintext_overrides() {
        fn resolved(raw: Option<String>) -> String {
            crate::auth::shared::sanitize_token_url(raw, TOKEN_URL)
        }

        // Unset or empty → the real production endpoint.
        assert_eq!(resolved(None), TOKEN_URL);
        assert_eq!(resolved(Some(String::new())), TOKEN_URL);
        // HTTPS to any host is fine — the refresh_token is encrypted in transit.
        assert_eq!(
            resolved(Some("https://mock.example/token".to_string())),
            "https://mock.example/token"
        );
        // Plain HTTP is allowed only for a loopback test mock.
        assert_eq!(
            resolved(Some("http://127.0.0.1:8080/token".to_string())),
            "http://127.0.0.1:8080/token"
        );
        assert_eq!(
            resolved(Some("http://localhost:9000/token".to_string())),
            "http://localhost:9000/token"
        );
        // Non-loopback plaintext would leak the refresh_token → rejected.
        assert_eq!(
            resolved(Some("http://evil.example/token".to_string())),
            TOKEN_URL
        );
        // A malformed value (no scheme/host) is rejected too.
        assert_eq!(resolved(Some("not a url".to_string())), TOKEN_URL);
    }

    #[test]
    fn applies_expiry_buffer_boundary() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let just_inside = token(1_299, None);
        let just_outside = token(1_301, None);

        assert!(!is_token_valid_at(&just_inside, now));
        assert!(is_token_valid_at(&just_outside, now));
    }

    #[test]
    fn parses_auth_json_for_api_key_mode() {
        let dir = std::env::temp_dir().join("shunt-auth-json-api-key");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            r#"{"auth_mode":"ApiKey","OPENAI_API_KEY":"key-from-file","tokens":null}"#,
        )
        .unwrap();

        assert_eq!(read_openai_api_key(&path).as_deref(), Some("key-from-file"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_auth_json_for_chatgpt_mode() {
        let access = token(2_000_000_000, Some("acct_claim"));
        let value = json!({
            "auth_mode": "ChatGPT",
            "OPENAI_API_KEY": null,
            "tokens": {"access_token": access, "refresh_token": "refresh"}
        });
        let auth = AuthFile { value };
        let tokens = auth.tokens().unwrap();

        assert_eq!(tokens.account_id().as_deref(), Some("acct_claim"));
    }

    #[test]
    fn parses_refresh_response_json() {
        let value = json!({
            "access_token": "access",
            "refresh_token": "refresh",
            "id_token": "id",
            "expires_in": 3600
        });

        let parsed = parse_refresh_response(&value).unwrap();

        assert_eq!(parsed.access_token, "access");
        assert_eq!(parsed.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(parsed.id_token.as_deref(), Some("id"));
    }

    #[test]
    fn token_set_is_valid_at_wraps_expiry_check() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let just_inside = TokenSet {
            access_token: token(1_299, None),
            refresh_token: None,
            account_id: None,
        };
        let just_outside = TokenSet {
            access_token: token(1_301, None),
            refresh_token: None,
            account_id: None,
        };

        assert!(!just_inside.is_valid_at(now));
        assert!(just_outside.is_valid_at(now));
    }

    #[test]
    fn token_set_to_credential_prefers_stored_account_id() {
        // The JWT carries a *different* account-id claim, so this genuinely proves
        // the stored account_id wins over the token claim (not just that one exists).
        let tokens = TokenSet {
            access_token: token(2_000_000_000, Some("acct_claim")),
            refresh_token: None,
            account_id: Some("acct_stored".to_string()),
        };

        let credential = tokens.to_credential().unwrap();

        assert_eq!(credential.access_token, tokens.access_token);
        assert_eq!(credential.account_id, "acct_stored");
    }

    #[test]
    fn token_set_to_credential_errors_without_account_id() {
        let tokens = TokenSet {
            access_token: token(2_000_000_000, None),
            refresh_token: None,
            account_id: None,
        };

        assert!(tokens.to_credential().is_err());
    }

    #[test]
    fn refresh_response_to_credential_reads_account_id_from_jwt() {
        let response = RefreshResponse {
            access_token: token(2_000_000_000, Some("acct_jwt")),
            refresh_token: None,
            id_token: None,
        };

        let credential = response.to_credential().unwrap();

        assert_eq!(credential.account_id, "acct_jwt");
    }

    #[test]
    fn refresh_response_to_credential_errors_without_claim() {
        let response = RefreshResponse {
            access_token: token(2_000_000_000, None),
            refresh_token: None,
            id_token: None,
        };

        assert!(response.to_credential().is_err());
    }

    #[test]
    fn parse_refresh_response_rejects_missing_access_token() {
        assert!(parse_refresh_response(&json!({"refresh_token": "r"})).is_none());
    }

    #[test]
    fn writeback_omitting_fields_keeps_existing_tokens() {
        let access = token(2_000_000_000, None);
        let mut value = json!({
            "tokens": {
                "access_token": "old",
                "refresh_token": "keep-refresh",
                "id_token": "keep-id",
                "account_id": "acct_kept"
            }
        });

        // A refresh that omits refresh_token/id_token and whose access token carries
        // no account-id claim must leave the previously stored values untouched.
        apply_refresh(
            &mut value,
            RefreshResponse {
                access_token: access.clone(),
                refresh_token: None,
                id_token: None,
            },
            UNIX_EPOCH + Duration::from_secs(0),
        );

        assert_eq!(value["tokens"]["access_token"], access);
        assert_eq!(value["tokens"]["refresh_token"], "keep-refresh");
        assert_eq!(value["tokens"]["id_token"], "keep-id");
        assert_eq!(value["tokens"]["account_id"], "acct_kept");
        assert_eq!(value["last_refresh"], "1970-01-01T00:00:00Z");
    }

    #[tokio::test]
    async fn concurrent_get_valid_single_flights_refresh() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let new_access = token(2_000_000_000, Some("acct_new"));
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(100))
                    .set_body_json(json!({
                        "access_token": new_access,
                        "refresh_token": "rotated-refresh",
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let dir = temp_auth_dir("single-flight");
        let path = dir.join("auth.json");
        write_auth(&path, &token(0, Some("acct_old")), "old-refresh");

        let first_store = CodexAuthStore::with_token_url(
            path.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
        );
        let second_store = CodexAuthStore::with_token_url(
            path.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
        );

        let (first, second) = tokio::join!(
            first_store.get_valid_chatgpt(),
            second_store.get_valid_chatgpt()
        );
        assert_eq!(first.unwrap().access_token, new_access);
        assert_eq!(second.unwrap().access_token, new_access);
        let stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored["tokens"]["refresh_token"], "rotated-refresh");
        server.verify().await;

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A [`wiremock::Respond`] that makes each incoming request block until a
    /// second one arrives at the same gate (or a bounded timeout elapses),
    /// then records whether the rendezvous happened. Used to deterministically
    /// prove two refreshes ran concurrently rather than being serialized
    /// behind a shared lock: if they were serialized, the second request would
    /// never reach the mock server until the first's entire refresh +
    /// writeback completed, so the first call's wait would time out instead
    /// of rendezvousing.
    struct ConcurrencyGate {
        count: StdMutex<usize>,
        condvar: Condvar,
    }

    impl ConcurrencyGate {
        fn new() -> Self {
            Self {
                count: StdMutex::new(0),
                condvar: Condvar::new(),
            }
        }

        /// Returns `true` only if `expected` arrivals were recorded before
        /// `timeout` elapsed.
        fn arrive_and_wait(&self, expected: usize, timeout: Duration) -> bool {
            let mut count = self.count.lock().unwrap_or_else(|error| error.into_inner());
            *count += 1;
            self.condvar.notify_all();
            let (_guard, result) = self
                .condvar
                .wait_timeout_while(count, timeout, |count| *count < expected)
                .unwrap();
            !result.timed_out()
        }
    }

    struct GatedResponder {
        gate: Arc<ConcurrencyGate>,
        arrivals: Arc<StdMutex<Vec<bool>>>,
        access_token: String,
    }

    impl wiremock::Respond for GatedResponder {
        fn respond(&self, _request: &wiremock::Request) -> wiremock::ResponseTemplate {
            let concurrent = self.gate.arrive_and_wait(2, Duration::from_secs(3));
            self.arrivals
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(concurrent);
            wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "access_token": self.access_token,
                "refresh_token": "rotated-refresh",
            }))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn different_paths_refresh_concurrently() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        // Each store's mock lives on its *own* `MockServer`: wiremock serializes
        // all request handling for a single server behind one internal lock
        // (see the "holding on to the write-side of the RwLock" note in its
        // hyper transport), so sharing one server between the two refreshes
        // would itself force them to serialize and produce a false failure
        // regardless of our path-keyed lock fix.
        let first_server = MockServer::start().await;
        let second_server = MockServer::start().await;
        let new_access = token(2_000_000_000, Some("acct_new"));
        let gate = Arc::new(ConcurrencyGate::new());
        let arrivals = Arc::new(StdMutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(GatedResponder {
                gate: gate.clone(),
                arrivals: arrivals.clone(),
                access_token: new_access.clone(),
            })
            .expect(1)
            .mount(&first_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(GatedResponder {
                gate: gate.clone(),
                arrivals: arrivals.clone(),
                access_token: new_access.clone(),
            })
            .expect(1)
            .mount(&second_server)
            .await;

        let first_dir = temp_auth_dir("parallel-paths-a");
        let first_path = first_dir.join("auth.json");
        write_auth(&first_path, &token(0, Some("acct_old_a")), "old-refresh-a");
        let second_dir = temp_auth_dir("parallel-paths-b");
        let second_path = second_dir.join("auth.json");
        write_auth(&second_path, &token(0, Some("acct_old_b")), "old-refresh-b");

        let first_store = CodexAuthStore::with_token_url(
            first_path.clone(),
            reqwest::Client::new(),
            format!("{}/token", first_server.uri()),
        );
        let second_store = CodexAuthStore::with_token_url(
            second_path.clone(),
            reqwest::Client::new(),
            format!("{}/token", second_server.uri()),
        );

        let (first, second) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(
                first_store.get_valid_chatgpt(),
                second_store.get_valid_chatgpt()
            )
        })
        .await
        .expect("refreshes for different credential paths must not deadlock");

        assert_eq!(first.unwrap().access_token, new_access);
        assert_eq!(second.unwrap().access_token, new_access);

        let recorded = {
            let recorded = arrivals.lock().unwrap_or_else(|error| error.into_inner());
            recorded.clone()
        };
        assert_eq!(
            recorded.len(),
            2,
            "both refreshes must reach the OAuth provider"
        );
        assert!(
            recorded.iter().all(|&concurrent| concurrent),
            "refreshes for different paths must run concurrently, not serialize: {recorded:?}"
        );

        first_server.verify().await;
        second_server.verify().await;
        let _ = std::fs::remove_dir_all(&first_dir);
        let _ = std::fs::remove_dir_all(&second_dir);
    }

    #[tokio::test]
    async fn cancelled_refresh_still_persists_rotated_token() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(200))
                    .set_body_json(json!({
                        "access_token": token(2_000_000_000, Some("acct_new")),
                        "refresh_token": "rotated-refresh",
                    })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let dir = temp_auth_dir("cancelled-refresh");
        let path = dir.join("auth.json");
        write_auth(&path, &token(0, Some("acct_old")), "old-refresh");

        let store = CodexAuthStore::with_token_url(
            path.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
        );

        let caller = tokio::spawn(async move { store.get_valid_chatgpt().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let requests = server
                    .received_requests()
                    .await
                    .expect("mock records requests");
                if requests.iter().any(|request| {
                    request.method.as_str() == "POST" && request.url.path() == "/token"
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("refresh request did not reach the OAuth provider");
        caller.abort();
        let error = caller.await.unwrap_err();
        assert!(error.is_cancelled());

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                if stored["tokens"]["refresh_token"] == "rotated-refresh" {
                    assert_eq!(
                        stored["tokens"]["access_token"],
                        token(2_000_000_000, Some("acct_new"))
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("detached refresh did not persist the rotated token");

        server.verify().await;
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn force_refresh_refreshes_a_still_valid_chatgpt_token() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let new_access = token(2_000_000_000, Some("acct_new"));
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": new_access,
                "refresh_token": "new-refresh",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let dir = std::env::temp_dir().join(format!(
            "shunt-codex-force-refresh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        // The stored access token still looks unexpired — proving force_refresh
        // bypasses the validity check `get_valid_chatgpt` performs.
        let still_valid = token(2_000_000_000, Some("acct_old"));
        std::fs::write(
            &path,
            json!({
                "auth_mode": "ChatGPT",
                "tokens": {
                    "access_token": still_valid,
                    "refresh_token": "old-refresh",
                }
            })
            .to_string(),
        )
        .unwrap();

        let store = CodexAuthStore::with_token_url(
            path.clone(),
            reqwest::Client::new(),
            format!("{}/token", server.uri()),
        );

        let credential = store.force_refresh().await.unwrap();

        assert_eq!(credential.access_token, new_access);
        assert_eq!(credential.account_id, "acct_new");
        let stored: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored["tokens"]["access_token"], new_access);
        assert_eq!(stored["tokens"]["refresh_token"], "new-refresh");
        server.verify().await;

        let _ = std::fs::remove_dir_all(dir);
    }

    // The redirect-hardening guard lives in `auth::shared::token_refresh_client`
    // and is shared by the Codex and Claude refresh paths; it is exercised here
    // through the Codex test module's existing wiremock scaffolding.
    #[tokio::test]
    async fn token_refresh_client_refuses_offhost_plaintext_redirect() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // A permitted (loopback) endpoint tries to bounce the refresh POST to a
        // plaintext off-host target; the hardened client must refuse to follow
        // rather than resend the refresh_token in the clear.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(307).insert_header("location", "http://evil.example/token"),
            )
            .mount(&server)
            .await;

        let error = crate::auth::shared::token_refresh_client()
            .post(format!("{}/token", server.uri()))
            .body("grant_type=refresh_token&refresh_token=secret")
            .send()
            .await
            .expect_err("must not follow a redirect to a plaintext off-host target");
        assert!(
            error.is_redirect(),
            "expected redirect refusal, got: {error}"
        );
    }

    #[tokio::test]
    async fn token_refresh_client_follows_safe_loopback_redirect() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // A 3xx to another loopback endpoint is still a safe target, so it is
        // followed — the guard blocks only unsafe (plaintext off-host) hops.
        let target = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string("followed"))
            .mount(&target)
            .await;
        let entry = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("location", format!("{}/token", target.uri())),
            )
            .mount(&entry)
            .await;

        let body = crate::auth::shared::token_refresh_client()
            .post(format!("{}/token", entry.uri()))
            .body("grant_type=refresh_token&refresh_token=secret")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "followed");
    }

    #[test]
    fn writeback_preserves_fields_and_updates_tokens() {
        let access = token(2_000_000_000, Some("acct_new"));
        let mut value = json!({
            "auth_mode": "ChatGPT",
            "OPENAI_API_KEY": "leave-me",
            "extra": {"kept": true},
            "tokens": {
                "access_token": "old",
                "refresh_token": "old-refresh",
                "id_token": "old-id",
                "account_id": "acct_old"
            },
            "last_refresh": "old"
        });

        apply_refresh(
            &mut value,
            RefreshResponse {
                access_token: access.clone(),
                refresh_token: Some("new-refresh".to_string()),
                id_token: Some("new-id".to_string()),
            },
            UNIX_EPOCH + Duration::from_secs(0),
        );

        assert_eq!(value["auth_mode"], "ChatGPT");
        assert_eq!(value["OPENAI_API_KEY"], "leave-me");
        assert_eq!(value["extra"]["kept"], true);
        assert_eq!(value["tokens"]["access_token"], access);
        assert_eq!(value["tokens"]["refresh_token"], "new-refresh");
        assert_eq!(value["tokens"]["id_token"], "new-id");
        assert_eq!(value["tokens"]["account_id"], "acct_new");
        assert_eq!(value["last_refresh"], "1970-01-01T00:00:00Z");
    }
}
