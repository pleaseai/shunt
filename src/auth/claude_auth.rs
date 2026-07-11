//! Claude subscription OAuth token source for the `shunt token` helper.
//!
//! Mirrors [`super::codex_auth`] but for the Claude Code login stored in
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

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const SCOPE: &str =
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

pub struct ClaudeAuthStore {
    path: PathBuf,
    client: reqwest::Client,
}

impl ClaudeAuthStore {
    pub fn new(path: PathBuf, client: reqwest::Client) -> Self {
        Self { path, client }
    }

    pub async fn get_valid_access_token(&self) -> anyhow::Result<String> {
        let tokens = self.read_tokens()?;
        if tokens.is_valid_at(SystemTime::now()) {
            return Ok(tokens.access_token);
        }

        // Re-read: a concurrent Claude Code / helper run may have just refreshed.
        let tokens = self.read_tokens()?;
        if tokens.is_valid_at(SystemTime::now()) {
            return Ok(tokens.access_token);
        }

        let refresh_token = tokens.refresh_token.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "no refresh token in {}; run `claude` then /login",
                self.path.display()
            )
        })?;
        let refreshed = refresh(&self.client, &refresh_token).await?;
        write_back(&self.path, &refreshed)?;
        Ok(refreshed.access_token)
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

async fn refresh(client: &reqwest::Client, refresh_token: &str) -> anyhow::Result<Refreshed> {
    let body = json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
        "client_id": CLIENT_ID,
        "scope": SCOPE,
    });
    let response = client
        .post(TOKEN_URL)
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
    write_atomic(path, &value)
}

fn write_atomic(path: &Path, value: &Value) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("credentials"),
        std::process::id()
    ));
    fs::write(&temp, serde_json::to_vec_pretty(value)?)?;
    set_private_permissions(&temp)?;
    fs::rename(&temp, path)?;
    set_private_permissions(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_when_beyond_expiry_buffer() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let inside = Tokens {
            access_token: "a".into(),
            refresh_token: None,
            expires_at_ms: (1_000 + 5 * 60 - 1) * 1000, // just inside the 5-min buffer
        };
        let outside = Tokens {
            access_token: "a".into(),
            refresh_token: None,
            expires_at_ms: (1_000 + 5 * 60 + 1) * 1000, // just outside
        };
        assert!(!inside.is_valid_at(now));
        assert!(outside.is_valid_at(now));
    }

    #[test]
    fn parses_credentials_tokens() {
        let value = json!({
            "claudeAiOauth": {
                "accessToken": "sk-ant-oat-access",
                "refreshToken": "sk-ant-ort-refresh",
                "expiresAt": 2_000_000_000_000i64,
                "subscriptionType": "max"
            }
        });
        let tokens = Tokens::from_value(&value).unwrap();
        assert_eq!(tokens.access_token, "sk-ant-oat-access");
        assert_eq!(tokens.refresh_token.as_deref(), Some("sk-ant-ort-refresh"));
        assert_eq!(tokens.expires_at_ms, 2_000_000_000_000);
    }

    #[test]
    fn refresh_reuses_prior_refresh_token_when_omitted() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let value = json!({"access_token": "new-access", "expires_in": 3600});
        let refreshed = parse_refresh(&value, "old-refresh", now).unwrap();
        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(refreshed.refresh_token, "old-refresh");
        assert_eq!(refreshed.expires_at_ms, 1_000 * 1000 + 3600 * 1000);
    }

    #[test]
    fn write_back_updates_tokens_and_preserves_other_fields() {
        let dir = std::env::temp_dir().join(format!(
            "shunt-claude-auth-{}",
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".credentials.json");
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"old-r","expiresAt":1,"subscriptionType":"max"},"mcpOAuth":{"keep":true}}"#,
        )
        .unwrap();

        write_back(
            &path,
            &Refreshed {
                access_token: "new".into(),
                refresh_token: "new-r".into(),
                expires_at_ms: 999,
            },
        )
        .unwrap();

        let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["claudeAiOauth"]["accessToken"], "new");
        assert_eq!(value["claudeAiOauth"]["refreshToken"], "new-r");
        assert_eq!(value["claudeAiOauth"]["expiresAt"], 999);
        assert_eq!(value["claudeAiOauth"]["subscriptionType"], "max"); // preserved
        assert_eq!(value["mcpOAuth"]["keep"], true); // preserved
        let _ = std::fs::remove_dir_all(dir);
    }
}
