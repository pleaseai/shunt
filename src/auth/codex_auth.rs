use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::adapters::AdapterError;
use crate::auth::auth_error;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const EXPIRY_BUFFER: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatGptCred {
    pub access_token: String,
    pub account_id: String,
}

#[derive(Debug, Clone)]
pub struct CodexAuthStore {
    path: PathBuf,
    client: reqwest::Client,
}

impl CodexAuthStore {
    pub fn new(path: PathBuf, client: reqwest::Client) -> Self {
        Self { path, client }
    }

    pub async fn get_valid_chatgpt(&self) -> Result<ChatGptCred, AdapterError> {
        let auth = self.read_auth_off_thread().await?;
        let tokens = auth
            .tokens()
            .ok_or_else(|| auth_error("ChatGPT auth tokens missing; run codex login"))?;
        if tokens.is_valid_at(SystemTime::now()) {
            return tokens.to_credential();
        }

        let auth = self.read_auth_off_thread().await?;
        let tokens = auth
            .tokens()
            .ok_or_else(|| auth_error("ChatGPT auth tokens missing; run codex login"))?;
        if tokens.is_valid_at(SystemTime::now()) {
            return tokens.to_credential();
        }

        let refresh_token = tokens
            .refresh_token
            .clone()
            .ok_or_else(|| auth_error("ChatGPT refresh token missing; run codex login"))?;
        let refreshed = refresh_tokens(&self.client, &refresh_token).await?;
        let credential = refreshed.to_credential()?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || write_refreshed_auth(&path, refreshed))
            .await
            .map_err(|error| auth_error(format!("ChatGPT auth write task failed: {error}")))?
            .map_err(|error| auth_error(format!("failed to update ChatGPT auth file: {error}")))?;
        Ok(credential)
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
    Some(UNIX_EPOCH + Duration::from_secs(seconds as u64))
}

pub fn jwt_account_id(token: &str) -> Option<String> {
    jwt_claims(token)?
        .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub fn is_token_valid_at(token: &str, now: SystemTime) -> bool {
    jwt_exp(token)
        .and_then(|exp| exp.checked_sub(EXPIRY_BUFFER))
        .is_some_and(|refresh_at| now < refresh_at)
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
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<RefreshResponse, AdapterError> {
    let response = client
        .post(TOKEN_URL)
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

pub(crate) fn write_auth_file_atomic(path: &Path, value: &Value) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("auth"),
        std::process::id()
    ));
    // The temp file must be born private: chmod-after-write would leave a
    // window where the tokens sit at the umask default on multi-user hosts.
    write_private(&temp, &serde_json::to_vec_pretty(value)?)?;
    fs::rename(&temp, path)?;
    set_private_permissions(path)?;
    Ok(())
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // `mode(0o600)` only applies when the file is created, so a stale or
    // pre-created temp at this predictable path would keep its old mode.
    // Remove any leftover, then require exclusive creation: if something
    // recreates the path in between, fail instead of writing tokens into a
    // file someone else owns.
    let _ = fs::remove_file(path);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::write(path, bytes)
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

#[cfg(test)]
mod tests {
    use super::*;

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
