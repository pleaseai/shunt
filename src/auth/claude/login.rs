//! `shunt login claude` — import a refreshable Claude Code login, run a full
//! refreshable OAuth flow, or store an inference-only long-lived setup token.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{bail, Context};
use serde::Deserialize;

pub(crate) use crate::auth::shared::{generate_pkce, PkceChallenge};

use super::{auth, callback::CallbackServer, store};

pub(crate) const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
pub(crate) const MANUAL_REDIRECT_URL: &str = "https://platform.claude.com/oauth/code/callback";
pub(crate) const SETUP_TOKEN_SCOPE: &str = "user:inference";
pub(crate) const SETUP_TOKEN_EXPIRES_SECS: u64 = 365 * 24 * 60 * 60;
const OAUTH_CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub async fn run(name: &str, long_lived: bool) -> anyhow::Result<()> {
    store::validate_account_name(name)?;
    let path = if long_lived {
        run_setup_token(name).await?
    } else {
        import_current_login(name).await?
    };
    println!(
        "Claude account {name:?} saved to {}. Add a name-only account entry to use it.",
        path.display()
    );
    Ok(())
}

/// Full refreshable OAuth login. By default, starts a loopback callback and
/// completes without a paste; `manual` forces the fixed manual redirect. Browser
/// launch and callback failures fall back to the manual-paste flow.
pub async fn run_oauth(name: &str, manual: bool) -> anyhow::Result<()> {
    store::validate_account_name(name)?;
    let path = if manual {
        run_oauth_manual(name).await?
    } else {
        match run_oauth_auto(name).await {
            Ok(path) => path,
            Err(error) => {
                eprintln!(
                    "Automatic Claude OAuth callback failed ({error}); falling back to manual paste."
                );
                run_oauth_manual(name).await?
            }
        }
    };
    println!(
        "Claude account {name:?} saved to {} (refreshable OAuth login). Add a name-only account entry to use it.",
        path.display()
    );
    Ok(())
}

async fn run_oauth_auto(name: &str) -> anyhow::Result<PathBuf> {
    let PkceChallenge {
        verifier,
        challenge,
        state,
    } = generate_pkce();
    let callback = CallbackServer::bind(state.clone()).await?;
    let redirect_uri = callback.redirect_uri();
    let authorize_url = build_authorize_url(&challenge, &state, auth::SCOPE, &redirect_uri)?;
    println!("Open this URL to authorize shunt with the Claude account to store:\n");
    println!("    {authorize_url}\n");
    open_url_async(authorize_url.as_str())
        .await
        .context("failed to open Claude OAuth authorization URL")?;
    let code = callback.wait_for_code(OAUTH_CALLBACK_TIMEOUT).await?;
    let tokens = exchange_code(
        &reqwest::Client::new(),
        &code,
        &state,
        &verifier,
        auth::TOKEN_URL,
        &redirect_uri,
        None,
    )
    .await?;
    persist_oauth_tokens(name, tokens)
}

async fn run_oauth_manual(name: &str) -> anyhow::Result<PathBuf> {
    let PkceChallenge {
        verifier,
        challenge,
        state,
    } = generate_pkce();
    let authorize_url = build_authorize_url(&challenge, &state, auth::SCOPE, MANUAL_REDIRECT_URL)?;
    println!("Open this URL to authorize shunt with the Claude account to store:\n");
    println!("    {authorize_url}\n");
    if let Err(error) = open_url_async(authorize_url.as_str()).await {
        eprintln!("Could not open browser automatically: {error}");
    }
    let pasted = prompt_authorization_code().await?;
    let (code, returned_state) = pasted
        .trim()
        .split_once('#')
        .ok_or_else(|| anyhow::anyhow!("authorization code must have the form <code>#<state>"))?;
    if code.is_empty() || returned_state != state {
        bail!("invalid Claude authorization code or OAuth state mismatch");
    }
    let tokens = exchange_code(
        &reqwest::Client::new(),
        code,
        &state,
        &verifier,
        auth::TOKEN_URL,
        MANUAL_REDIRECT_URL,
        None,
    )
    .await?;
    persist_oauth_tokens(name, tokens)
}

fn persist_oauth_tokens(name: &str, tokens: TokenExchangeResponse) -> anyhow::Result<PathBuf> {
    let refresh_token = tokens
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Claude token exchange did not return a refresh token"))?;
    let account_uuid = tokens
        .account
        .as_ref()
        .map(|account| account.uuid.as_str())
        .filter(|uuid| !uuid.is_empty());
    if account_uuid.is_none() {
        eprintln!("Warning: Claude token exchange did not return an account UUID; the account_uuid rewrite will be skipped for this account.");
    }
    let expires_at_ms = oauth_expires_at_ms(tokens.expires_in);
    store::store_oauth_tokens(
        name,
        &tokens.access_token,
        refresh_token,
        expires_at_ms,
        account_uuid,
    )
}

/// Convert the token response's `expires_in` (seconds) to an absolute epoch-ms
/// `expiresAt`, mirroring `ClaudeAuthStore`'s refresh math (default 3600s).
pub(crate) fn oauth_expires_at_ms(expires_in: Option<i64>) -> i64 {
    // Only an absent lifetime falls back to the 1-hour default. An explicit
    // non-positive `expires_in` yields an already-expired timestamp (secs = 0) so
    // the refresh path runs before the token is first sent upstream, rather than
    // silently granting a spurious one-hour lifetime.
    let secs = match expires_in {
        None => 3600,
        Some(value) => value.max(0),
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    // Saturate rather than overflow: a pathologically large `expires_in` (e.g. a
    // buggy or hostile token response) must not panic or wrap into the past.
    now_ms.saturating_add(secs.saturating_mul(1000))
}

async fn import_current_login(name: &str) -> anyhow::Result<PathBuf> {
    let source = auth::default_credentials_path();
    let metadata_source = claude_global_config_path();
    let account_uuid = tokio::task::spawn_blocking(move || {
        read_current_account_uuid(&metadata_source).with_context(|| {
            "failed to read the current Claude account UUID; run `claude auth login` again"
        })
    })
    .await
    .context("Claude account metadata read task failed")??;
    let name = name.to_string();
    let source_display = source.display().to_string();
    tokio::task::spawn_blocking(move || {
        store::import_credentials(&name, &source, Some(&account_uuid)).with_context(|| {
            format!(
                "failed to import {source_display}; run `claude auth login` first, or use `shunt login claude --name {name} --long-lived`"
            )
        })
    })
    .await
    .context("Claude credential import task failed")?
}

async fn run_setup_token(name: &str) -> anyhow::Result<PathBuf> {
    let PkceChallenge {
        verifier,
        challenge,
        state,
    } = generate_pkce();
    let authorize_url =
        build_authorize_url(&challenge, &state, SETUP_TOKEN_SCOPE, MANUAL_REDIRECT_URL)?;

    println!("Open this URL to authorize shunt with the Claude account to store:\n");
    println!("    {authorize_url}\n");
    if let Err(error) = open_url_async(authorize_url.as_str()).await {
        eprintln!("Could not open browser automatically: {error}");
    }
    let pasted = prompt_authorization_code().await?;
    let (code, returned_state) = pasted
        .trim()
        .split_once('#')
        .ok_or_else(|| anyhow::anyhow!("authorization code must have the form <code>#<state>"))?;
    if code.is_empty() || returned_state != state {
        bail!("invalid Claude authorization code or OAuth state mismatch");
    }

    let tokens = exchange_code(
        &reqwest::Client::new(),
        code,
        &state,
        &verifier,
        auth::TOKEN_URL,
        MANUAL_REDIRECT_URL,
        Some(SETUP_TOKEN_EXPIRES_SECS),
    )
    .await?;
    let account_uuid = tokens
        .account
        .as_ref()
        .map(|account| account.uuid.as_str())
        .filter(|uuid| !uuid.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Claude token exchange did not return an account UUID"))?;
    store::store_setup_token(name, &tokens.access_token, Some(account_uuid))
}

pub(crate) fn build_authorize_url(
    challenge: &str,
    state: &str,
    scope: &str,
    redirect_uri: &str,
) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(AUTHORIZE_URL)?;
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", auth::CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", scope)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    Ok(url)
}

#[derive(Debug, Deserialize)]
pub(crate) struct TokenExchangeResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
    pub account: Option<TokenAccount>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct TokenAccount {
    pub uuid: String,
}

pub(crate) async fn exchange_code(
    client: &reqwest::Client,
    code: &str,
    state: &str,
    verifier: &str,
    token_url: &str,
    redirect_uri: &str,
    expires_in: Option<u64>,
) -> anyhow::Result<TokenExchangeResponse> {
    let mut body = serde_json::Map::from_iter([
        (
            "grant_type".to_string(),
            serde_json::json!("authorization_code"),
        ),
        ("code".to_string(), serde_json::json!(code)),
        ("redirect_uri".to_string(), serde_json::json!(redirect_uri)),
        ("client_id".to_string(), serde_json::json!(auth::CLIENT_ID)),
        ("code_verifier".to_string(), serde_json::json!(verifier)),
        ("state".to_string(), serde_json::json!(state)),
    ]);
    if let Some(expires_in) = expires_in {
        body.insert("expires_in".to_string(), serde_json::json!(expires_in));
    }
    let body = serde_json::Value::Object(body);
    let response = client
        .post(token_url)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&body)?)
        .send()
        .await
        .context("failed to exchange Claude authorization code")?;
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail: String = text.chars().take(200).collect();
        bail!("Claude token exchange failed ({status}): {detail}");
    }
    serde_json::from_str(&text).context("invalid Claude token exchange response")
}

fn read_current_account_uuid(path: &Path) -> anyhow::Result<String> {
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)
        .with_context(|| format!("invalid JSON in {}", path.display()))?;
    value
        .pointer("/oauthAccount/accountUuid")
        .and_then(serde_json::Value::as_str)
        .filter(|uuid| !uuid.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("no oauthAccount.accountUuid in {}", path.display()))
}

fn claude_global_config_path() -> PathBuf {
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|path| !path.is_empty());
    let home = std::env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|path| !path.is_empty()));
    claude_global_config_path_from(config_dir.as_deref(), home.as_deref())
}

fn claude_global_config_path_from(
    config_dir: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> PathBuf {
    let home = home.map(PathBuf::from).unwrap_or_default();
    let config_home = config_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));
    let legacy = config_home.join(".config.json");
    if legacy.exists() {
        legacy
    } else {
        config_dir
            .map(PathBuf::from)
            .unwrap_or(home)
            .join(".claude.json")
    }
}

fn open_url(url: &str) -> anyhow::Result<()> {
    let status = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()?
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .status()?
    } else {
        std::process::Command::new("xdg-open").arg(url).status()?
    };
    if !status.success() {
        bail!("browser open command exited with {status}");
    }
    Ok(())
}

/// Launch the browser without blocking the async runtime: `open_url` spawns a
/// child process and waits on it, so it runs on `spawn_blocking`'s dedicated pool
/// rather than a Tokio worker thread.
async fn open_url_async(url: &str) -> anyhow::Result<()> {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || open_url(&url))
        .await
        .context("Claude OAuth browser open task failed")?
}

/// Prompt for the pasted authorization code without blocking the async runtime.
/// `rpassword::prompt_password` blocks on terminal input (potentially for
/// minutes), so it is offloaded to `spawn_blocking`.
async fn prompt_authorization_code() -> anyhow::Result<String> {
    tokio::task::spawn_blocking(|| {
        rpassword::prompt_password(
            "Paste the authorization code shown after approval (input hidden): ",
        )
    })
    .await
    .context("Claude authorization code prompt task failed")?
    .context("failed to read Claude authorization code")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_json, body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn authorization_url_requests_inference_only_with_pkce() {
        let url = build_authorize_url("challenge", "state", SETUP_TOKEN_SCOPE, MANUAL_REDIRECT_URL)
            .unwrap();
        let params = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            params.get("scope").map(|value| value.as_ref()),
            Some("user:inference")
        );
        assert_eq!(
            params.get("code_challenge").map(|value| value.as_ref()),
            Some("challenge")
        );
        assert_eq!(
            params.get("state").map(|value| value.as_ref()),
            Some("state")
        );
        assert_eq!(
            params.get("redirect_uri").map(|value| value.as_ref()),
            Some(MANUAL_REDIRECT_URL)
        );
    }

    #[test]
    fn authorization_url_requests_full_oauth_scope() {
        let url =
            build_authorize_url("challenge", "state", auth::SCOPE, MANUAL_REDIRECT_URL).unwrap();
        let params = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            params.get("scope").map(|value| value.as_ref()),
            Some(auth::SCOPE)
        );
    }

    #[test]
    fn oauth_expires_at_defaults_absent_and_clamps_non_positive() {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        // An absent lifetime falls back to ~1 hour ahead.
        assert!(oauth_expires_at_ms(None) >= before + 3600 * 1000 - 2000);
        // A positive lifetime lands that many seconds ahead.
        assert!(oauth_expires_at_ms(Some(7200)) >= before + 7200 * 1000 - 2000);
        // An explicit zero or negative lifetime is immediately expired (not the
        // 1-hour default), so it never lands meaningfully in the future.
        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        assert!(oauth_expires_at_ms(Some(0)) <= after);
        assert!(oauth_expires_at_ms(Some(-10)) <= after);
        // A pathologically large lifetime saturates instead of overflowing.
        assert_eq!(oauth_expires_at_ms(Some(i64::MAX)), i64::MAX);
    }

    #[test]
    fn read_current_account_uuid_extracts_and_validates() {
        let dir = std::env::temp_dir().join(format!(
            "shunt-account-uuid-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let good = dir.join("good.json");
        std::fs::write(&good, r#"{"oauthAccount":{"accountUuid":"acc-123"}}"#).unwrap();
        assert_eq!(read_current_account_uuid(&good).unwrap(), "acc-123");

        // Missing accountUuid, empty accountUuid, and invalid JSON all error.
        let missing = dir.join("missing.json");
        std::fs::write(&missing, r#"{"oauthAccount":{}}"#).unwrap();
        assert!(read_current_account_uuid(&missing).is_err());

        let empty = dir.join("empty.json");
        std::fs::write(&empty, r#"{"oauthAccount":{"accountUuid":""}}"#).unwrap();
        assert!(read_current_account_uuid(&empty).is_err());

        let invalid = dir.join("invalid.json");
        std::fs::write(&invalid, "not json at all").unwrap();
        assert!(read_current_account_uuid(&invalid).is_err());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn resolves_default_and_custom_claude_global_config_paths() {
        let root = std::env::temp_dir().join(format!(
            "shunt-claude-config-path-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let custom = root.join("custom");
        std::fs::create_dir_all(&custom).unwrap();

        assert_eq!(
            claude_global_config_path_from(None, Some(root.as_os_str())),
            root.join(".claude.json")
        );
        assert_eq!(
            claude_global_config_path_from(Some(custom.as_os_str()), Some(root.as_os_str())),
            custom.join(".claude.json")
        );

        std::fs::write(custom.join(".config.json"), "{}").unwrap();
        assert_eq!(
            claude_global_config_path_from(Some(custom.as_os_str()), Some(root.as_os_str())),
            custom.join(".config.json")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn token_exchange_returns_issuing_account_uuid() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_json(json!({
                "grant_type": "authorization_code",
                "code": "code",
                "redirect_uri": MANUAL_REDIRECT_URL,
                "client_id": auth::CLIENT_ID,
                "code_verifier": "verifier",
                "state": "state",
                "expires_in": SETUP_TOKEN_EXPIRES_SECS,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "long-lived",
                "account": {"uuid": "account-two"},
                "organization": {"uuid": "org-two"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let response = exchange_code(
            &reqwest::Client::new(),
            "code",
            "state",
            "verifier",
            &format!("{}/token", server.uri()),
            MANUAL_REDIRECT_URL,
            Some(SETUP_TOKEN_EXPIRES_SECS),
        )
        .await
        .unwrap();
        assert_eq!(response.access_token, "long-lived");
        assert_eq!(response.account.unwrap().uuid, "account-two");
    }

    #[tokio::test]
    async fn oauth_exchange_sends_loopback_redirect_uri() {
        let server = MockServer::start().await;
        let redirect_uri = "http://localhost:45678/callback";
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_partial_json(json!({
                "redirect_uri": redirect_uri,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "oauth-access",
                "refresh_token": "oauth-refresh",
                "expires_in": 3600
            })))
            .expect(1)
            .mount(&server)
            .await;

        let response = exchange_code(
            &reqwest::Client::new(),
            "oauth-code",
            "oauth-state",
            "oauth-verifier",
            &format!("{}/token", server.uri()),
            redirect_uri,
            None,
        )
        .await
        .unwrap();
        assert_eq!(response.refresh_token.as_deref(), Some("oauth-refresh"));
    }

    #[tokio::test]
    async fn oauth_exchange_persists_refreshable_tokens() {
        let _guard = store::TEST_ENV_LOCK.lock().await;
        let server = MockServer::start().await;
        let dir = std::env::temp_dir().join(format!(
            "shunt-claude-oauth-login-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let token_url = format!("{}/token", server.uri());
        let _accounts_env =
            crate::auth::shared::EnvVarGuard::set("SHUNT_CLAUDE_ACCOUNTS_DIR", &dir);
        let _token_env =
            crate::auth::shared::EnvVarGuard::set("SHUNT_CLAUDE_TOKEN_URL", &token_url);
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_json(json!({
                "grant_type": "authorization_code",
                "code": "oauth-code",
                "redirect_uri": MANUAL_REDIRECT_URL,
                "client_id": auth::CLIENT_ID,
                "code_verifier": "oauth-verifier",
                "state": "oauth-state",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "oauth-access",
                "refresh_token": "oauth-refresh",
                "expires_in": 7200,
                "account": {"uuid": "oauth-account"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let response = exchange_code(
            &reqwest::Client::new(),
            "oauth-code",
            "oauth-state",
            "oauth-verifier",
            &std::env::var("SHUNT_CLAUDE_TOKEN_URL").unwrap(),
            MANUAL_REDIRECT_URL,
            None,
        )
        .await
        .unwrap();
        assert_eq!(response.refresh_token.as_deref(), Some("oauth-refresh"));
        assert_eq!(response.expires_in, Some(7200));

        let path = persist_oauth_tokens("oauth", response).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["claudeAiOauth"]["accessToken"], "oauth-access");
        assert_eq!(value["claudeAiOauth"]["refreshToken"], "oauth-refresh");
        assert!(value["claudeAiOauth"]["expiresAt"].as_i64().unwrap() > 0);
        assert_eq!(value["shuntAccountUuid"], "oauth-account");
        assert!(value["claudeAiOauth"].get("shuntCredentialKind").is_none());
        let meta = store::account_meta("oauth").expect("OAuth account metadata parses");
        assert!(matches!(meta.kind, store::AccountKind::Imported));

        let _ = std::fs::remove_dir_all(dir);
    }
}
