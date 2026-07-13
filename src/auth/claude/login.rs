//! `shunt login claude` — import a refreshable Claude Code login or run an
//! inference-only OAuth flow that stores a long-lived token with its account UUID.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{auth, store};

const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
const MANUAL_REDIRECT_URL: &str = "https://platform.claude.com/oauth/code/callback";
const SETUP_TOKEN_EXPIRES_SECS: u64 = 365 * 24 * 60 * 60;

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
    let verifier = random_urlsafe(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_urlsafe(32);
    let authorize_url = build_authorize_url(&challenge, &state)?;

    println!("Open this URL to authorize shunt with the Claude account to store:\n");
    println!("    {authorize_url}\n");
    if let Err(error) = open_url(authorize_url.as_str()) {
        eprintln!("Could not open browser automatically: {error}");
    }
    let pasted = rpassword::prompt_password(
        "Paste the authorization code shown after approval (input hidden): ",
    )
    .context("failed to read Claude authorization code")?;
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

fn build_authorize_url(challenge: &str, state: &str) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(AUTHORIZE_URL)?;
    url.query_pairs_mut()
        .append_pair("code", "true")
        .append_pair("client_id", auth::CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", MANUAL_REDIRECT_URL)
        .append_pair("scope", "user:inference")
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    Ok(url)
}

fn random_urlsafe(bytes: usize) -> String {
    let mut random = vec![0_u8; bytes];
    rand::rng().fill_bytes(&mut random);
    URL_SAFE_NO_PAD.encode(random)
}

#[derive(Debug, Deserialize)]
struct TokenExchangeResponse {
    access_token: String,
    account: Option<TokenAccount>,
}

#[derive(Debug, Deserialize)]
struct TokenAccount {
    uuid: String,
}

async fn exchange_code(
    client: &reqwest::Client,
    code: &str,
    state: &str,
    verifier: &str,
    token_url: &str,
) -> anyhow::Result<TokenExchangeResponse> {
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": MANUAL_REDIRECT_URL,
        "client_id": auth::CLIENT_ID,
        "code_verifier": verifier,
        "state": state,
        "expires_in": SETUP_TOKEN_EXPIRES_SECS,
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn authorization_url_requests_inference_only_with_pkce() {
        let url = build_authorize_url("challenge", "state").unwrap();
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
        )
        .await
        .unwrap();
        assert_eq!(response.access_token, "long-lived");
        assert_eq!(response.account.unwrap().uuid, "account-two");
    }
}
