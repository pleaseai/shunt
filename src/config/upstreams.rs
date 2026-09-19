use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use super::{
    expand_tilde, presets, AccountConfig, ApiKeyHeader, AuthMode, ConfigError, CountTokens,
    ProviderConfig, ProviderKind, ProvidersConfig, RetryConfig,
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
    /// See [`ProviderConfig::service_tier`]; Codex CLI's "Fast" mode opt-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// See [`ProviderConfig::classifier_model`] (`kind = "anthropic"` only);
    /// unset by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier_model: Option<String>,
    #[serde(default)]
    pub count_tokens: CountTokens,
    #[serde(default)]
    pub websocket: bool,
    /// See [`ProviderConfig::tool_search`] for semantics; unset ("auto") by
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_search: Option<bool>,
    /// zstd-compress Responses request bodies for this upstream (issue #285).
    /// On by default; only effective on the ChatGPT/Codex flavor.
    #[serde(default = "super::default_true")]
    pub request_compression: bool,
    #[serde(default)]
    pub retry: RetryConfig,
    /// See [`ProviderConfig::workspace_roots`] (`kind = "antigravity"` only).
    /// Empty by default: no prompt-derived working directory is honored.
    #[serde(default)]
    pub workspace_roots: Vec<String>,
    /// See [`ProviderConfig::profile_dir`] (`kind = "antigravity_cli"` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_dir: Option<String>,
    /// See [`ProviderConfig::sandbox`] (`kind = "antigravity"` only). On by
    /// default; an ordered upstream must be able to opt out for the same
    /// reasons a `[providers.*]` entry can.
    #[serde(default = "super::default_true")]
    pub sandbox: bool,
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
        /// Absent leaves the header alone, preserving a preset's choice (the
        /// `opencode` preset sends `x-api-key`; an env-only map must not flip
        /// it back to bearer, which zen rejects at request time).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        header: Option<ApiKeyHeader>,
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
    /// Kimi Code subscription OAuth. Shaped like `ClaudeOauth`/`ChatgptOauth`
    /// because it goes through the same `absorb_oauth_scope` path onto
    /// `provider.account_scope`/`provider.accounts`, which the Kimi account
    /// pool (`resolve_pool_accounts`) reads to build its candidate list;
    /// `resolve_kimi_account` resolves one account per call, same as the
    /// Claude/ChatGPT resolvers it mirrors.
    KimiOauth {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accounts: Option<Vec<AccountSelection>>,
    },
    XaiOauth {},
    CursorOauth {},
    AntigravityOauth {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accounts: Option<Vec<AccountSelection>>,
    },
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
                if let Some(header) = header {
                    provider.api_key_header = header;
                }
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
            Self::Map(AuthMap::KimiOauth { account, accounts }) => {
                absorb_oauth_scope(upstream, AuthMode::KimiOauth, account, accounts, provider)?;
            }
            Self::Map(AuthMap::XaiOauth {}) => provider.auth = AuthMode::XaiOauth,
            Self::Map(AuthMap::CursorOauth {}) => provider.auth = AuthMode::CursorOauth,
            Self::Map(AuthMap::AntigravityOauth { account, accounts }) => {
                absorb_oauth_scope(
                    upstream,
                    AuthMode::AntigravityOauth,
                    account,
                    accounts,
                    provider,
                )?;
            }
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
        (None, Some(accounts)) if accounts.is_empty() => {
            return Err(ConfigError::EmptyUpstreamAccountList {
                upstream: upstream.to_string(),
            });
        }
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
            api_key_header: preset.map_or(super::ApiKeyHeader::default(), |preset| {
                preset.api_key_header.unwrap_or_default()
            }),
            effort: upstream.effort.clone(),
            service_tier: upstream.service_tier.clone(),
            classifier_model: upstream.classifier_model.clone(),
            count_tokens: upstream.count_tokens,
            accounts: Vec::new(),
            account_scope: Vec::new(),
            websocket: upstream.websocket,
            tool_search: upstream.tool_search,
            request_compression: upstream.request_compression,
            retry: upstream.retry,
            workspace_roots: upstream.workspace_roots.clone(),
            profile_dir: upstream.profile_dir.clone().map(|dir| expand_tilde(&dir)),
            sandbox: upstream.sandbox,
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

#[cfg(test)]
mod antigravity_tests {
    use super::*;
    use figment::{
        providers::{Format, Yaml},
        Figment,
    };

    fn upstream(auth: &str) -> UpstreamConfig {
        Figment::from(Yaml::string(&format!(
            "name: agy\nkind: antigravity\nbase_url: http://localhost\nauth: {auth}\n"
        )))
        .extract()
        .unwrap()
    }

    #[test]
    fn antigravity_yaml_accounts_normalize_and_validate() {
        let mut config = super::super::Config::default();
        config.server.default_provider = "agy".into();
        config.upstreams = vec![upstream(
            "{mode: antigravity_oauth, accounts: [stored, {name: inline, credentials: /tmp/agy.json, priority: 2, disabled: true}]}"
        )];
        let config = config.validate().unwrap();
        let provider = config.provider("agy").unwrap();
        assert_eq!(provider.auth, AuthMode::AntigravityOauth);
        assert_eq!(provider.account_scope, ["stored"]);
        assert_eq!(provider.accounts.len(), 1);
        assert_eq!(provider.accounts[0].name, "inline");
        assert_eq!(
            provider.accounts[0].credentials.as_deref(),
            Some("/tmp/agy.json")
        );
        assert_eq!(provider.accounts[0].priority, 2);
        assert!(provider.accounts[0].disabled);
    }

    #[test]
    fn antigravity_yaml_single_account_and_shorthand() {
        for (auth, scope) in [
            ("antigravity_oauth", vec![]),
            ("{mode: antigravity_oauth}", vec![]),
            (
                "{mode: antigravity_oauth, account: primary}",
                vec!["primary"],
            ),
        ] {
            let (providers, _) = normalize(&[upstream(auth)]).unwrap();
            assert_eq!(providers["agy"].auth, AuthMode::AntigravityOauth);
            assert_eq!(providers["agy"].account_scope, scope);
            assert!(providers["agy"].accounts.is_empty());
        }
    }

    #[test]
    fn antigravity_toml_accounts_round_trip() {
        let upstream: UpstreamConfig = toml::from_str(
            r#"name = "agy"
kind = "antigravity"
base_url = "http://localhost"
auth = { mode = "antigravity_oauth", accounts = ["primary", { name = "backup", token_env = "AGY_BACKUP", threshold = 0.8 }] }
"#,
        )
        .unwrap();
        let encoded = toml::to_string(&upstream).unwrap();
        let decoded: UpstreamConfig = toml::from_str(&encoded).unwrap();
        let (providers, _) = normalize(&[decoded]).unwrap();
        let provider = &providers["agy"];
        assert_eq!(provider.auth, AuthMode::AntigravityOauth);
        assert_eq!(provider.account_scope, ["primary"]);
        assert_eq!(
            provider.accounts[0].token_env.as_deref(),
            Some("AGY_BACKUP")
        );
        assert_eq!(provider.accounts[0].threshold, Some(0.8));
    }

    #[test]
    fn antigravity_account_validation_rejects_invalid_and_duplicate_names() {
        for selections in [
            "['../escape']",
            "['Bad']",
            "[{name: '../escape'}]",
            "[primary, primary]",
            "[primary, {name: primary}]",
            "[{name: primary}, {name: primary}]",
        ] {
            let mut config = super::super::Config::default();
            config.server.default_provider = "agy".into();
            config.upstreams = vec![upstream(&format!(
                "{{mode: antigravity_oauth, accounts: {selections}}}"
            ))];
            assert!(matches!(
                config.validate().unwrap_err(),
                ConfigError::InvalidAccountName { .. } | ConfigError::DuplicateAccountName { .. }
            ));
        }
    }

    #[test]
    fn antigravity_account_validation_rejects_conflicting_sources_and_thresholds() {
        for (fields, multiple_sources) in [
            ("credentials: /tmp/agy.json, token_env: AGY_TOKEN", true),
            ("threshold: 1.1", false),
            ("threshold_5h: -0.1", false),
            ("threshold_7d: 1.1", false),
            ("threshold_fable: -0.1", false),
        ] {
            let mut config = super::super::Config::default();
            config.server.default_provider = "agy".into();
            config.upstreams = vec![upstream(&format!(
                "{{mode: antigravity_oauth, accounts: [{{name: primary, {fields}}}]}}"
            ))];
            let error = config.validate().unwrap_err();
            if multiple_sources {
                assert!(matches!(
                    error,
                    ConfigError::AccountMultipleCredentialSources { .. }
                ));
            } else {
                assert!(matches!(error, ConfigError::InvalidAccountThreshold { .. }));
            }
        }
    }

    #[test]
    fn antigravity_yaml_selection_errors_are_explicit() {
        for (auth, expected) in [
            (
                "{mode: antigravity_oauth, account: one, accounts: [two]}",
                "at most one",
            ),
            (
                "{mode: antigravity_oauth, accounts: []}",
                "explicitly empty",
            ),
            ("{mode: antigravity_oauth, account: ' '}", "non-whitespace"),
        ] {
            assert!(normalize(&[upstream(auth)])
                .unwrap_err()
                .to_string()
                .contains(expected));
        }
    }
}
