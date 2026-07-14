//! Claude subscription OAuth token source for the `shunt token` helper.
//!
//! Mirrors [`crate::auth::codex::auth`] but for the Claude Code login stored in
//! `~/.claude/.credentials.json` under `claudeAiOauth`. Reads the access token,
//! and when it is within a 5-minute buffer of `expiresAt`, refreshes it via the
//! same grant Claude Code itself uses (`platform.claude.com/v1/oauth/token`,
//! verified against the leaked Claude Code source `src/services/oauth/client.ts`),
//! then writes the new token back atomically, preserving every other field.
//!
//! Used to feed `apiKeyHelper`, so gateway model discovery fires (it needs a
//! gateway credential) while Claude passthrough keeps billing to the subscription.
//! Claude Code sends an `apiKeyHelper` value in both `x-api-key` and
//! `Authorization: Bearer`; the OAuth token in `x-api-key` would be rejected by
//! api.anthropic.com, so the passthrough adapter strips that duplicate (see
//! `strip_duplicate_oauth_api_key` in [`crate::adapters::anthropic`]). Without
//! it, this helper would satisfy discovery and mapped routes only, not passthrough.

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::shared::write_auth_file_atomic;

pub(crate) const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub(crate) const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub(crate) const SCOPE: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const EXPIRY_BUFFER: Duration = Duration::from_secs(5 * 60);

/// Resolve a usable token: a static override if present, else the refreshed
/// subscription access token.
pub async fn resolve_token(path: PathBuf, client: reqwest::Client) -> anyhow::Result<String> {
    if let Some(token) = static_override() {
        return Ok(token);
    }
    ClaudeAuthStore::new(path, client)
        .get_valid_access_token()
        .await
}

/// Static-mode override: a long-lived `claude setup-token` value supplied by env.
pub fn static_override() -> Option<String> {
    for var in ["SHUNT_GATEWAY_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN"] {
        if let Ok(value) = env::var(var) {
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

pub fn default_credentials_path() -> PathBuf {
    env::var_os("CLAUDE_CREDENTIALS")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".claude").join(".credentials.json"))
        })
        .unwrap_or_else(|| PathBuf::from(".claude/.credentials.json"))
}

/// Resolve the Claude OAuth refresh endpoint from a raw `SHUNT_CLAUDE_TOKEN_URL`
/// value. Parity with the Codex store (#118): a non-loopback plaintext override
/// would egress the long-lived `refresh_token` in the clear, so it is rejected in
/// favor of the production endpoint. Shares
/// [`crate::auth::shared::sanitize_token_url`] with the Codex store so the egress
/// guard cannot drift between them.
fn sanitize_token_url(raw: Option<String>) -> String {
    crate::auth::shared::sanitize_token_url(raw, TOKEN_URL)
}

#[derive(Clone)]
pub struct ClaudeAuthStore {
    path: PathBuf,
    client: reqwest::Client,
    token_url: String,
}

/// In-process single-flight for Claude OAuth refreshes. Stores are constructed
/// per request, so the lock must be shared across independent instances. The
/// refresh task owns the guard through atomic writeback, preventing a cancelled
/// caller from exposing an already-consumed refresh token to another request.
static REFRESH_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

impl ClaudeAuthStore {
    pub fn new(path: PathBuf, client: reqwest::Client) -> Self {
        // `SHUNT_CLAUDE_TOKEN_URL` overrides the OAuth refresh endpoint. It exists
        // so the refresh path can be pointed at a local mock in tests; production
        // deployments leave it unset and use the real platform.claude.com URL.
        // `sanitize_token_url` rejects a non-loopback plaintext override so a
        // misconfigured env var can never egress the long-lived `refresh_token`
        // off-origin or in the clear (parity with the Codex store, #118).
        let token_url = sanitize_token_url(env::var("SHUNT_CLAUDE_TOKEN_URL").ok());
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

    pub async fn get_valid_access_token(&self) -> anyhow::Result<String> {
        let tokens = self.read_tokens_off_thread().await?;
        if tokens.is_valid_at(SystemTime::now()) {
            return Ok(tokens.access_token);
        }

        // Single-flight the refresh. A waiter re-reads after acquiring the lock,
        // so it uses the token persisted by the caller that refreshed first.
        let refreshing = REFRESH_LOCK.lock().await;
        let tokens = self.read_tokens_off_thread().await?;
        if tokens.is_valid_at(SystemTime::now()) {
            return Ok(tokens.access_token);
        }

        self.refresh_and_write_back(tokens, refreshing).await
    }

    /// Refresh and persist the stored OAuth token regardless of its expiry.
    pub async fn force_refresh(&self) -> anyhow::Result<String> {
        let refreshing = REFRESH_LOCK.lock().await;
        let tokens = self.read_tokens_off_thread().await?;
        self.refresh_and_write_back(tokens, refreshing).await
    }

    /// Refresh and persist the stored OAuth token if the file still contains the
    /// access token rejected by the upstream. If another request has already
    /// refreshed it, return that newer token without rotating again.
    pub async fn force_refresh_if_access_token(
        &self,
        rejected_access_token: &str,
    ) -> anyhow::Result<String> {
        let refreshing = REFRESH_LOCK.lock().await;
        let tokens = self.read_tokens_off_thread().await?;
        if tokens.access_token != rejected_access_token {
            return Ok(tokens.access_token);
        }
        self.refresh_and_write_back(tokens, refreshing).await
    }

    async fn refresh_and_write_back(
        &self,
        tokens: Tokens,
        refreshing: tokio::sync::MutexGuard<'static, ()>,
    ) -> anyhow::Result<String> {
        let refresh_token = tokens.refresh_token.ok_or_else(|| {
            anyhow::anyhow!(
                "no refresh token in {}; run `claude` then /login",
                self.path.display()
            )
        })?;

        // The detached task owns both the single-flight guard and the critical
        // refresh + writeback sequence. Dropping the caller's future therefore
        // cannot strand a rotated refresh token in memory after the provider has
        // consumed the old one.
        let client = self.client.clone();
        let token_url = self.token_url.clone();
        let path = self.path.clone();
        tokio::spawn(async move {
            let _refreshing = refreshing;
            let refreshed = refresh(&client, &token_url, &refresh_token).await?;
            let access_token = refreshed.access_token.clone();
            if let Err(error) = write_back_off_thread(path, refreshed).await {
                // The upstream refresh already consumed the old refresh token, so a
                // failed writeback leaves the file holding a now-invalid token. Log
                // here (not only via the returned Err) so the failure stays visible
                // even when the caller's future was dropped and the JoinHandle —
                // carrying this Err — is discarded with it.
                tracing::warn!(
                    %error,
                    "Claude OAuth token refreshed upstream but writeback failed; stored refresh token is now stale until re-login"
                );
                return Err(anyhow::anyhow!("failed to update Claude auth file: {error}"));
            }
            Ok::<String, anyhow::Error>(access_token)
        })
        .await
        .map_err(|error| anyhow::anyhow!("Claude refresh task failed: {error}"))?
    }

    /// Read the credential file on Tokio's blocking pool so synchronous file I/O
    /// never stalls an async runtime worker.
    async fn read_tokens_off_thread(&self) -> anyhow::Result<Tokens> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.read_tokens())
            .await
            .map_err(|error| anyhow::anyhow!("Claude auth read task failed: {error}"))?
    }

    fn read_tokens(&self) -> anyhow::Result<Tokens> {
        let value = read_file(&self.path).map_err(|error| {
            anyhow::anyhow!(
                "cannot read {} ({error}); run `claude` then /login, or use SHUNT_GATEWAY_TOKEN",
                self.path.display()
            )
        })?;
        Tokens::from_value(&value)
            .ok_or_else(|| anyhow::anyhow!("no claudeAiOauth tokens in {}", self.path.display()))
    }
}

struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    expires_at_ms: i64,
}

impl Tokens {
    fn from_value(value: &Value) -> Option<Self> {
        let oauth = value.get("claudeAiOauth")?;
        Some(Tokens {
            access_token: oauth.get("accessToken")?.as_str()?.to_string(),
            refresh_token: oauth
                .get("refreshToken")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned),
            expires_at_ms: oauth.get("expiresAt").and_then(Value::as_i64).unwrap_or(0),
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

struct Refreshed {
    access_token: String,
    refresh_token: String,
    expires_at_ms: i64,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

fn parse_refresh(value: &Value, refresh_token: &str, now: SystemTime) -> Option<Refreshed> {
    let parsed: RefreshResponse = serde_json::from_value(value.clone()).ok()?;
    let now_ms = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    Some(Refreshed {
        access_token: parsed.access_token,
        // The backend may omit refresh_token on a refresh grant -> reuse the
        // existing one (matches Claude Code's own `newRefreshToken = refreshToken`).
        refresh_token: parsed
            .refresh_token
            .unwrap_or_else(|| refresh_token.to_string()),
        expires_at_ms: now_ms + parsed.expires_in.unwrap_or(3600) * 1000,
    })
}

async fn refresh(
    // The injected proxy client follows redirects freely; the refresh POST
    // carries the long-lived refresh_token, so it goes through the
    // redirect-hardened `token_refresh_client()` instead — a permitted token
    // endpoint must not be able to 3xx the credential to a plaintext/off-loopback
    // host and defeat the initial-URL-only `sanitize_token_url` guard.
    _client: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
) -> anyhow::Result<Refreshed> {
    let body = json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
        "scope": SCOPE,
    });
    let response = crate::auth::shared::token_refresh_client()
        .post(token_url)
        .header("content-type", "application/json")
        .body(serde_json::to_string(&body)?)
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail: String = text.chars().take(200).collect();
        anyhow::bail!("token refresh failed ({status}): {detail}");
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| anyhow::anyhow!("invalid refresh response: {error}"))?;
    parse_refresh(&value, refresh_token, SystemTime::now())
        .ok_or_else(|| anyhow::anyhow!("refresh response missing access_token"))
}

fn read_file(path: &Path) -> io::Result<Value> {
    serde_json::from_slice(&fs::read(path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn write_back(path: &Path, refreshed: &Refreshed) -> io::Result<()> {
    let mut value = read_file(path)?;
    let oauth = value
        .get_mut("claudeAiOauth")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "claudeAiOauth missing"))?;
    oauth.insert("accessToken".to_string(), json!(refreshed.access_token));
    oauth.insert("refreshToken".to_string(), json!(refreshed.refresh_token));
    oauth.insert("expiresAt".to_string(), json!(refreshed.expires_at_ms));
    write_auth_file_atomic(path, &value)
}

/// Persist the refreshed credential on Tokio's blocking pool. The on-disk
/// content and atomic write semantics remain those of [`write_back`].
async fn write_back_off_thread(path: PathBuf, refreshed: Refreshed) -> io::Result<()> {
    tokio::task::spawn_blocking(move || write_back(&path, &refreshed))
        .await
        .map_err(|error| io::Error::other(format!("Claude auth write task failed: {error}")))?
}

#[cfg(test)]
mod tests;
