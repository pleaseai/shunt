use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::{AuthMode, Config, ConfigError, ProviderKind};
use crate::routing::strip_context_window_hint;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WeeklyFallbackConfig {
    pub enabled: bool,
    pub claude_provider: String,
    pub codex_provider: String,
    pub models: Vec<WeeklyFallbackModel>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WeeklyFallbackModel {
    pub claude: String,
    pub codex: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_fallback: Option<String>,
}

impl WeeklyFallbackConfig {
    pub(super) fn validate(&self, config: &Config) -> Result<(), ConfigError> {
        if !self.enabled {
            return Ok(());
        }
        for (field, name, kind, auth) in [
            (
                "claude_provider",
                &self.claude_provider,
                ProviderKind::Anthropic,
                AuthMode::ClaudeOauth,
            ),
            (
                "codex_provider",
                &self.codex_provider,
                ProviderKind::Responses,
                AuthMode::ChatgptOauth,
            ),
        ] {
            if name.trim().is_empty() {
                return Err(invalid(format!("{field} must not be empty")));
            }
            let provider = config
                .provider(name)
                .ok_or_else(|| invalid(format!("{field} references unknown provider {name}")))?;
            if provider.kind != kind || provider.auth != auth {
                return Err(invalid(format!(
                    "{field} provider {name} must use kind {kind:?} and auth {auth:?}"
                )));
            }
        }
        if self.models.is_empty() {
            return Err(invalid(
                "models must contain at least one backend model pair",
            ));
        }
        let mut claude_models = HashSet::new();
        let mut codex_models = HashSet::new();
        for (index, pair) in self.models.iter().enumerate() {
            for (field, model) in [("claude", &pair.claude), ("codex", &pair.codex)]
                .into_iter()
                .chain(
                    pair.claude_fallback
                        .as_ref()
                        .map(|model| ("claude_fallback", model)),
                )
            {
                if model.is_empty()
                    || model.chars().any(|c| c.is_whitespace() || c.is_control())
                    || strip_context_window_hint(model) != model
                {
                    return Err(invalid(format!(
                        "models[{index}].{field} must be a non-empty backend model ID without whitespace, control characters, or a context-window hint"
                    )));
                }
            }
            if !claude_models.insert(&pair.claude) {
                return Err(invalid(format!("duplicate claude model {}", pair.claude)));
            }
            if !codex_models.insert(&pair.codex) {
                return Err(invalid(format!("duplicate codex model {}", pair.codex)));
            }
            if pair.claude_fallback.as_ref() == Some(&pair.claude) {
                return Err(invalid(format!(
                    "models[{index}].claude_fallback must differ from claude"
                )));
            }
        }
        for model in &config.models {
            if model.upstream_model.as_ref().is_some_and(|upstreams| {
                upstreams.len() > 1
                    && (upstreams.contains_key(&self.claude_provider)
                        || upstreams.contains_key(&self.codex_provider))
            }) {
                return Err(invalid(format!(
                    "model {} has a generic route chain that contains a weekly fallback provider",
                    model.id
                )));
            }
        }
        Ok(())
    }
}

fn invalid(message: impl Into<String>) -> ConfigError {
    ConfigError::InvalidWeeklyFallback {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests;
