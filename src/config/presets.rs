use super::{AuthMode, ProviderKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderPresetView {
    pub name: &'static str,
    pub auth: AuthMode,
    pub api_key_env: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ProviderPreset {
    pub name: &'static str,
    pub kind: ProviderKind,
    pub base_url: &'static str,
    pub auth: AuthMode,
    pub api_key_env: Option<&'static str>,
}

pub(super) const PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        name: "anthropic",
        kind: ProviderKind::Anthropic,
        base_url: "https://api.anthropic.com",
        auth: AuthMode::Passthrough,
        api_key_env: None,
    },
    ProviderPreset {
        name: "codex",
        kind: ProviderKind::Responses,
        base_url: "https://chatgpt.com/backend-api",
        auth: AuthMode::ChatgptOauth,
        api_key_env: None,
    },
    ProviderPreset {
        name: "openai",
        kind: ProviderKind::Responses,
        base_url: "https://api.openai.com/v1",
        auth: AuthMode::ApiKey,
        api_key_env: Some("OPENAI_API_KEY"),
    },
    ProviderPreset {
        name: "xai",
        kind: ProviderKind::Responses,
        base_url: "https://api.x.ai/v1",
        auth: AuthMode::ApiKey,
        api_key_env: Some("XAI_API_KEY"),
    },
    ProviderPreset {
        name: "grok",
        kind: ProviderKind::Responses,
        base_url: "https://cli-chat-proxy.grok.com/v1",
        auth: AuthMode::XaiOauth,
        api_key_env: None,
    },
    ProviderPreset {
        name: "kimi",
        kind: ProviderKind::Anthropic,
        base_url: "https://api.moonshot.ai/anthropic",
        auth: AuthMode::ApiKey,
        api_key_env: Some("MOONSHOT_API_KEY"),
    },
    ProviderPreset {
        name: "cursor",
        kind: ProviderKind::Cursor,
        base_url: "https://api2.cursor.sh",
        auth: AuthMode::CursorOauth,
        api_key_env: None,
    },
];

pub(super) fn find(name: &str) -> Option<&'static ProviderPreset> {
    PRESETS.iter().find(|preset| preset.name == name)
}

pub(super) fn available_names() -> String {
    PRESETS
        .iter()
        .map(|preset| preset.name)
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn provider_presets() -> impl ExactSizeIterator<Item = ProviderPresetView> {
    PRESETS.iter().map(|preset| ProviderPresetView {
        name: preset.name,
        auth: preset.auth,
        api_key_env: preset.api_key_env,
    })
}

#[cfg(test)]
mod tests {
    use super::{available_names, find, provider_presets, PRESETS};
    use crate::config::{AuthMode, ProviderKind};

    #[test]
    fn preset_table_contains_the_supported_backends_in_documented_order() {
        assert_eq!(
            PRESETS.iter().map(|preset| preset.name).collect::<Vec<_>>(),
            [
                "anthropic",
                "codex",
                "openai",
                "xai",
                "grok",
                "kimi",
                "cursor"
            ]
        );
        assert_eq!(
            available_names(),
            "anthropic, codex, openai, xai, grok, kimi, cursor"
        );
    }

    #[test]
    fn public_view_exposes_only_init_scaffolding_metadata() {
        let views = provider_presets().collect::<Vec<_>>();
        assert_eq!(views.len(), PRESETS.len());
        assert_eq!(views[0].name, "anthropic");
        assert_eq!(views[0].auth, AuthMode::Passthrough);
        assert_eq!(views[0].api_key_env, None);
        assert_eq!(views[2].name, "openai");
        assert_eq!(views[2].auth, AuthMode::ApiKey);
        assert_eq!(views[2].api_key_env, Some("OPENAI_API_KEY"));
    }

    #[test]
    fn kimi_preset_uses_the_moonshot_anthropic_surface() {
        let kimi = find("kimi").unwrap();
        assert_eq!(kimi.kind, ProviderKind::Anthropic);
        assert_eq!(kimi.base_url, "https://api.moonshot.ai/anthropic");
        assert_eq!(kimi.auth, AuthMode::ApiKey);
        assert_eq!(kimi.api_key_env, Some("MOONSHOT_API_KEY"));
    }
}
