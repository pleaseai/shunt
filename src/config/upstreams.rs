use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use super::{
    presets, AccountConfig, ApiKeyHeader, AuthMode, ConfigError, CountTokens, ProviderConfig,
    ProviderKind, ProvidersConfig, RetryConfig,
};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ProviderKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<UpstreamAuth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default)]
    pub count_tokens: CountTokens,
    #[serde(default)]
    pub websocket: bool,
    #[serde(default)]
    pub tool_search: bool,
    #[serde(default)]
    pub retry: RetryConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum UpstreamAuth {
    Shorthand(AuthMode),
    Map(AuthMap),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthMap {
    Passthrough {},
    ApiKey {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<String>,
        #[serde(default)]
        header: ApiKeyHeader,
    },
    ClaudeOauth {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accounts: Option<Vec<AccountSelection>>,
    },
    ChatgptOauth {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accounts: Option<Vec<AccountSelection>>,
    },
    XaiOauth {},
    CursorOauth {},
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum AccountSelection {
    Reference(String),
    Inline(AccountConfig),
}

impl UpstreamAuth {
    fn absorb(self, upstream: &str, provider: &mut ProviderConfig) -> Result<(), ConfigError> {
        match self {
            Self::Shorthand(mode) => provider.auth = mode,
            Self::Map(AuthMap::Passthrough {}) => provider.auth = AuthMode::Passthrough,
            Self::Map(AuthMap::ApiKey { env, header }) => {
                provider.auth = AuthMode::ApiKey;
                if env.is_some() {
                    provider.api_key_env = env;
                }
                provider.api_key_header = header;
            }
            Self::Map(AuthMap::ClaudeOauth { account, accounts }) => {
                absorb_oauth_scope(upstream, AuthMode::ClaudeOauth, account, accounts, provider)?;
            }
            Self::Map(AuthMap::ChatgptOauth { account, accounts }) => {
                absorb_oauth_scope(
                    upstream,
                    AuthMode::ChatgptOauth,
                    account,
                    accounts,
                    provider,
                )?;
            }
            Self::Map(AuthMap::XaiOauth {}) => provider.auth = AuthMode::XaiOauth,
            Self::Map(AuthMap::CursorOauth {}) => provider.auth = AuthMode::CursorOauth,
        }
        Ok(())
    }
}

fn absorb_oauth_scope(
    upstream: &str,
    mode: AuthMode,
    account: Option<String>,
    accounts: Option<Vec<AccountSelection>>,
    provider: &mut ProviderConfig,
) -> Result<(), ConfigError> {
    if account.is_some() && accounts.is_some() {
        return Err(ConfigError::UpstreamAuthAccountConflict {
            upstream: upstream.to_string(),
        });
    }
    provider.auth = mode;
    let selections = match (account, accounts) {
        (Some(name), None) => vec![AccountSelection::Reference(name)],
        (None, Some(accounts)) => accounts,
        (None, None) => Vec::new(),
        (Some(_), Some(_)) => unreachable!("account xor accounts was checked"),
    };
    for selection in selections {
        match selection {
            AccountSelection::Reference(name) => {
                if name.trim().is_empty() {
                    return Err(ConfigError::EmptyUpstreamAccountReference {
                        upstream: upstream.to_string(),
                    });
                }
                provider.account_scope.push(name);
            }
            AccountSelection::Inline(account) => provider.accounts.push(account),
        }
    }
    Ok(())
}

pub(super) fn normalize(
    upstreams: &[UpstreamConfig],
) -> Result<(ProvidersConfig, Vec<String>), ConfigError> {
    let mut providers = BTreeMap::new();
    let mut order = Vec::with_capacity(upstreams.len());
    let mut names = HashSet::new();

    for (index, upstream) in upstreams.iter().enumerate() {
        if upstream.name.trim().is_empty() {
            return Err(ConfigError::EmptyUpstreamName { index });
        }
        if !names.insert(upstream.name.as_str()) {
            return Err(ConfigError::DuplicateUpstreamName {
                name: upstream.name.clone(),
            });
        }
        let preset = upstream
            .provider
            .as_deref()
            .map(|name| {
                presets::find(name).ok_or_else(|| ConfigError::UnknownProviderPreset {
                    upstream: upstream.name.clone(),
                    preset: name.to_string(),
                    available: presets::available_names(),
                })
            })
            .transpose()?;
        let kind = upstream
            .kind
            .or_else(|| preset.map(|preset| preset.kind))
            .ok_or_else(|| ConfigError::MissingUpstreamKind {
                upstream: upstream.name.clone(),
            })?;
        let base_url = upstream
            .base_url
            .clone()
            .or_else(|| preset.map(|preset| preset.base_url.to_string()))
            .ok_or_else(|| ConfigError::MissingUpstreamBaseUrl {
                upstream: upstream.name.clone(),
            })?;
        let mut provider = ProviderConfig {
            kind,
            base_url,
            auth: preset.map_or(AuthMode::Passthrough, |preset| preset.auth),
            api_key_env: preset.and_then(|preset| preset.api_key_env.map(str::to_string)),
            api_key_header: ApiKeyHeader::default(),
            effort: upstream.effort.clone(),
            count_tokens: upstream.count_tokens,
            accounts: Vec::new(),
            account_scope: Vec::new(),
            websocket: upstream.websocket,
            tool_search: upstream.tool_search,
            retry: upstream.retry,
        };
        if let Some(auth) = upstream.auth.clone() {
            auth.absorb(&upstream.name, &mut provider)?;
        }
        order.push(upstream.name.clone());
        providers.insert(upstream.name.clone(), provider);
    }

    Ok((providers, order))
}

#[cfg(test)]
mod tests;
