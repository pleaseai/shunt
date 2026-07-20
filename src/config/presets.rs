use super::{AuthMode, ProviderKind};

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

#[cfg(test)]
mod tests {
    use super::{available_names, find, PRESETS};
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
    fn kimi_preset_uses_the_moonshot_anthropic_surface() {
        let kimi = find("kimi").unwrap();
        assert_eq!(kimi.kind, ProviderKind::Anthropic);
        assert_eq!(kimi.base_url, "https://api.moonshot.ai/anthropic");
        assert_eq!(kimi.auth, AuthMode::ApiKey);
        assert_eq!(kimi.api_key_env, Some("MOONSHOT_API_KEY"));
    }
}
