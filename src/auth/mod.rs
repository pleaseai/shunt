use std::{env, path::PathBuf};

use axum::{http::StatusCode, response::IntoResponse};

use crate::{adapters::AdapterError, config::Config, error::ShuntError, routing::Route};

pub mod claude_auth;
pub mod codex_auth;

// TODO(M2): Add the optional `shunt login` PKCE loopback fallback. M2 currently
// reuses the Codex CLI-owned ~/.codex/auth.json credential source.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    ApiKey(String),
    ChatGptOAuth {
        access_token: String,
        account_id: String,
    },
}

pub async fn resolve_credential(
    config: &Config,
    route: &Route,
    client: &reqwest::Client,
) -> Result<Credential, AdapterError> {
    match route.provider.as_str() {
        "openai" => resolve_openai(config).map(Credential::ApiKey),
        "codex" | "chatgpt" => {
            let store = codex_auth::CodexAuthStore::new(default_codex_auth_path(), client.clone());
            store
                .get_valid_chatgpt()
                .await
                .map(|credential| Credential::ChatGptOAuth {
                    access_token: credential.access_token,
                    account_id: credential.account_id,
                })
        }
        provider => Err(auth_error(format!(
            "responses adapter does not support provider {provider}"
        ))),
    }
}

pub fn resolve_openai(config: &Config) -> Result<String, AdapterError> {
    if let Ok(value) = env::var("OPENAI_API_KEY") {
        if !value.is_empty() {
            return Ok(value);
        }
    }

    if let Some(value) = codex_auth::read_openai_api_key(&default_codex_auth_path()) {
        return Ok(value);
    }

    env::var(&config.providers.openai.api_key_env).map_err(|_| {
        auth_error(format!(
            "{} is not set",
            config.providers.openai.api_key_env
        ))
    })
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

#[cfg(test)]
mod tests {
    use crate::{
        config::Config,
        routing::{AdapterKind, Route},
    };

    use super::resolve_openai;

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

        let key = resolve_openai(&Config::default()).unwrap();

        assert_eq!(key, "file-key");
        std::env::remove_var("CODEX_AUTH_FILE");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn codex_route_uses_responses_auth_path() {
        let route = Route {
            provider: "codex".to_string(),
            adapter: AdapterKind::Responses,
            model: "gpt-5.2-codex".to_string(),
            upstream_model: "gpt-5.2-codex".to_string(),
            effort: None,
        };

        assert_eq!(route.provider, "codex");
    }
}
