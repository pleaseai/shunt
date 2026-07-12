use std::{env, path::PathBuf};

use axum::{http::StatusCode, response::IntoResponse};

use crate::{
    adapters::AdapterError,
    config::{ApiKeyHeader, AuthMode, Config, ProviderConfig},
    error::ShuntError,
    routing::Route,
};

pub mod claude_auth;
pub mod codex_auth;
pub mod cursor_auth;
pub mod cursor_login;
pub mod inbound;
pub mod xai_auth;
pub mod xai_login;

// TODO(M2): Add the optional `shunt login` PKCE loopback fallback. M2 currently
// reuses the Codex CLI-owned ~/.codex/auth.json credential source.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// Forward the client's own credential unchanged (Anthropic passthrough).
    Passthrough,
    /// Inject an API key, sent in the given header.
    ApiKey { value: String, header: ApiKeyHeader },
    ChatGptOAuth {
        access_token: String,
        account_id: String,
    },
    /// xAI subscription OAuth: bearer only, no account-id header.
    XaiOauth { access_token: String },
    /// Cursor OAuth bearer.
    CursorOauth { access_token: String },
}

/// Resolve the credential for a route from its provider's configured `auth`.
pub async fn resolve_credential(
    config: &Config,
    route: &Route,
    client: &reqwest::Client,
) -> Result<Credential, AdapterError> {
    let provider = config
        .provider(&route.provider)
        .ok_or_else(|| auth_error(format!("unknown provider {}", route.provider)))?;
    match provider.auth {
        AuthMode::Passthrough => Ok(Credential::Passthrough),
        AuthMode::ApiKey => Ok(Credential::ApiKey {
            value: resolve_api_key(&route.provider, provider)?,
            header: provider.api_key_header,
        }),
        AuthMode::ChatgptOauth => {
            let store = codex_auth::CodexAuthStore::new(default_codex_auth_path(), client.clone());
            store
                .get_valid_chatgpt()
                .await
                .map(|credential| Credential::ChatGptOAuth {
                    access_token: credential.access_token,
                    account_id: credential.account_id,
                })
        }
        AuthMode::CursorOauth => {
            let base_url = env::var("SHUNT_CURSOR_BASE_URL")
                .ok()
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| provider.base_url.clone());
            let store = cursor_auth::CursorAuthStore::new(
                default_cursor_auth_path(),
                client.clone(),
                base_url,
            );
            store
                .get_valid()
                .await
                .map(|credential| Credential::CursorOauth {
                    access_token: credential.access_token,
                })
        }
        AuthMode::XaiOauth => {
            let store = xai_auth::XaiAuthStore::new(default_xai_auth_path(), client.clone());
            store
                .get_valid()
                .await
                .map(|credential| Credential::XaiOauth {
                    access_token: credential.access_token,
                })
        }
    }
}

/// Read an `auth = "api_key"` provider's key from its `api_key_env`. As a
/// convenience the built-in OpenAI provider also falls back to the key inside
/// ~/.codex/auth.json when `OPENAI_API_KEY` is unset.
fn resolve_api_key(name: &str, provider: &ProviderConfig) -> Result<String, AdapterError> {
    let env_name = provider.api_key_env.as_deref().ok_or_else(|| {
        auth_error(format!(
            "provider {name} uses auth = \"api_key\" but api_key_env is not set"
        ))
    })?;

    if let Ok(value) = env::var(env_name) {
        if !value.is_empty() {
            return Ok(value);
        }
    }

    if env_name == "OPENAI_API_KEY" {
        if let Some(value) = codex_auth::read_openai_api_key(&default_codex_auth_path()) {
            return Ok(value);
        }
    }

    Err(auth_error(format!("{env_name} is not set")))
}

pub fn auth_error(message: impl Into<String>) -> AdapterError {
    let error = ShuntError::new(StatusCode::UNAUTHORIZED, "authentication_error", message);
    AdapterError {
        message: "authentication failed".to_string(),
        response: Box::new(error.into_response()),
    }
}

fn default_codex_auth_path() -> PathBuf {
    env::var_os("CODEX_AUTH_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".codex").join("auth.json"))
        })
        .unwrap_or_else(|| PathBuf::from(".codex/auth.json"))
}

pub fn default_cursor_auth_path() -> PathBuf {
    env::var_os("SHUNT_CURSOR_AUTH_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            // `HOME` is unset on Windows, where `shunt login cursor` is supported;
            // fall back to `USERPROFILE` so the credential lands in the user's home
            // rather than a working-directory-relative path.
            env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .or_else(|| env::var_os("USERPROFILE").filter(|home| !home.is_empty()))
                .map(PathBuf::from)
                .map(|home| home.join(".shunt").join("cursor-auth.json"))
        })
        .unwrap_or_else(|| PathBuf::from(".shunt/cursor-auth.json"))
}

/// shunt-owned xAI credential file: `$SHUNT_XAI_AUTH_FILE`, else
/// `~/.shunt/xai-auth.json`. Unlike the codex path this file is written by
/// `shunt login xai` and refreshed by shunt alone.
pub fn default_xai_auth_path() -> PathBuf {
    env::var_os("SHUNT_XAI_AUTH_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".shunt").join("xai-auth.json"))
        })
        .unwrap_or_else(|| PathBuf::from(".shunt/xai-auth.json"))
}

#[cfg(test)]
mod tests {
    use crate::config::Config;

    use super::resolve_api_key;

    #[test]
    fn resolves_openai_key_from_codex_auth_json_when_env_missing() {
        let dir = std::env::temp_dir().join(format!(
            "shunt-auth-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let auth_file = dir.join("auth.json");
        std::fs::write(
            &auth_file,
            r#"{"auth_mode":"ApiKey","OPENAI_API_KEY":"file-key","tokens":null}"#,
        )
        .unwrap();
        std::env::remove_var("OPENAI_API_KEY");
        std::env::set_var("CODEX_AUTH_FILE", &auth_file);

        let config = Config::default();
        let key = resolve_api_key("openai", config.provider("openai").unwrap()).unwrap();

        assert_eq!(key, "file-key");
        std::env::remove_var("CODEX_AUTH_FILE");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn api_key_provider_requires_env_var() {
        let config = Config::default();
        // A fresh temp env with no key set and no codex fallback for a non-openai
        // env var name must error rather than silently pass.
        std::env::remove_var("SHUNT_TEST_MISSING_KEY");
        let mut provider = config.provider("openai").unwrap().clone();
        provider.api_key_env = Some("SHUNT_TEST_MISSING_KEY".to_string());
        assert!(resolve_api_key("kimi", &provider).is_err());
    }
}
