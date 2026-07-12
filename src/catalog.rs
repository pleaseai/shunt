//! Built-in provider catalog for `shunt add provider`.
//!
//! `add provider <name>` prints a ready-to-paste `[providers.<name>]` TOML
//! block for a known backend — the built-in providers plus the Anthropic-
//! compatible gateways shunt documents. The output is valid TOML (the
//! setup guidance rides along as comments), so it pipes straight into a config
//! file: `shunt add provider kimi >> shunt.toml`. It never writes files itself,
//! mirroring `flue add`'s print-only philosophy.
//!
//! The catalog is a static table (AGENTS.md's table-driven rule): a new backend
//! is one [`CatalogEntry`], no branching logic. Rendering maps the typed config
//! enums to their wire tokens via exhaustive `match`, so a new [`AuthMode`] /
//! [`ProviderKind`] / [`ApiKeyHeader`] variant forces this file to be updated,
//! and `catalog_entries_render_to_valid_config` round-trips every entry through
//! the real config loader so a bad `base_url`/`auth` can't ship.

use std::fmt::Write as _;

use crate::config::{ApiKeyHeader, AuthMode, ProviderKind};

/// One known upstream: the fields needed to scaffold a `[providers.<name>]`
/// block, plus optional setup guidance rendered as comments.
pub struct CatalogEntry {
    /// Canonical provider name (the config table key).
    pub name: &'static str,
    /// Extra names that resolve to this entry (e.g. `zai` → `glm`).
    pub aliases: &'static [&'static str],
    pub kind: ProviderKind,
    pub base_url: &'static str,
    pub auth: AuthMode,
    /// Env var holding the API key, for `auth = "api_key"` entries.
    pub api_key_env: Option<&'static str>,
    /// Non-default header for the injected key. `None` ⇒ the `bearer` default,
    /// so the line is omitted.
    pub api_key_header: Option<ApiKeyHeader>,
    /// Example model ID for the commented route (`None` ⇒ a `<model-id>`
    /// placeholder).
    pub example_model: Option<&'static str>,
    /// One-line setup hint (e.g. an OAuth login step) rendered as a comment.
    pub note: Option<&'static str>,
}

/// The known providers: the built-ins that ship as config defaults, then the
/// Anthropic-compatible gateways shunt documents. Order is the listing
/// order for `shunt add provider` with no name.
pub const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        name: "anthropic",
        aliases: &[],
        kind: ProviderKind::Anthropic,
        base_url: "https://api.anthropic.com",
        auth: AuthMode::Passthrough,
        api_key_env: None,
        api_key_header: None,
        example_model: None,
        note: Some("Passthrough forwards Claude Code's own Anthropic credential unchanged."),
    },
    CatalogEntry {
        name: "openai",
        aliases: &[],
        kind: ProviderKind::Responses,
        base_url: "https://api.openai.com/v1",
        auth: AuthMode::ApiKey,
        api_key_env: Some("OPENAI_API_KEY"),
        api_key_header: None,
        example_model: None,
        note: None,
    },
    CatalogEntry {
        name: "codex",
        aliases: &["chatgpt"],
        kind: ProviderKind::Responses,
        base_url: "https://chatgpt.com/backend-api",
        auth: AuthMode::ChatgptOauth,
        api_key_env: None,
        api_key_header: None,
        example_model: None,
        note: Some("Reuse your ChatGPT/Codex subscription — run `codex login` first."),
    },
    CatalogEntry {
        name: "xai",
        aliases: &[],
        kind: ProviderKind::Responses,
        base_url: "https://api.x.ai/v1",
        auth: AuthMode::ApiKey,
        api_key_env: Some("XAI_API_KEY"),
        api_key_header: None,
        example_model: Some("grok-code-fast-1"),
        note: Some("Developer API, billed per token. For a SuperGrok / X Premium+ subscription use `grok`."),
    },
    CatalogEntry {
        name: "grok",
        aliases: &[],
        kind: ProviderKind::Responses,
        base_url: "https://cli-chat-proxy.grok.com/v1",
        auth: AuthMode::XaiOauth,
        api_key_env: None,
        api_key_header: None,
        example_model: Some("grok-code-fast-1"),
        note: Some(
            "Reuse your SuperGrok / X Premium+ subscription — run `shunt login xai` first.",
        ),
    },
    CatalogEntry {
        name: "kimi",
        aliases: &["moonshot"],
        kind: ProviderKind::Anthropic,
        base_url: "https://api.moonshot.ai/anthropic",
        auth: AuthMode::ApiKey,
        api_key_env: Some("KIMI_API_KEY"),
        api_key_header: None,
        example_model: Some("kimi-k2.7-code"),
        note: None,
    },
    CatalogEntry {
        name: "deepseek",
        aliases: &[],
        kind: ProviderKind::Anthropic,
        base_url: "https://api.deepseek.com/anthropic",
        auth: AuthMode::ApiKey,
        api_key_env: Some("DEEPSEEK_API_KEY"),
        api_key_header: None,
        example_model: Some("deepseek-v4-pro"),
        note: None,
    },
    CatalogEntry {
        name: "glm",
        aliases: &["zai", "z.ai"],
        kind: ProviderKind::Anthropic,
        base_url: "https://api.z.ai/api/anthropic",
        auth: AuthMode::ApiKey,
        api_key_env: Some("ZAI_API_KEY"),
        api_key_header: None,
        example_model: Some("glm-5.2"),
        note: None,
    },
    CatalogEntry {
        name: "minimax",
        aliases: &[],
        kind: ProviderKind::Anthropic,
        base_url: "https://api.minimax.io/anthropic",
        auth: AuthMode::ApiKey,
        api_key_env: Some("MINIMAX_API_KEY"),
        api_key_header: None,
        example_model: None,
        note: Some("See https://platform.minimax.io/docs/token-plan/claude-code for model IDs."),
    },
    CatalogEntry {
        name: "mimo",
        aliases: &["xiaomi"],
        kind: ProviderKind::Anthropic,
        base_url: "https://api.xiaomimimo.com/anthropic",
        auth: AuthMode::ApiKey,
        api_key_env: Some("MIMO_API_KEY"),
        api_key_header: None,
        example_model: Some("mimo-v2.5-pro"),
        note: Some(
            "See https://mimo.mi.com/docs/en-US/tokenplan/integration/claudecode for model IDs.",
        ),
    },
    CatalogEntry {
        name: "openrouter",
        aliases: &[],
        kind: ProviderKind::Anthropic,
        base_url: "https://openrouter.ai/api",
        auth: AuthMode::ApiKey,
        api_key_env: Some("OPENROUTER_API_KEY"),
        api_key_header: None,
        example_model: Some("anthropic/claude-opus-4.8"),
        note: None,
    },
    CatalogEntry {
        name: "vercel",
        aliases: &["vercel-ai-gateway"],
        kind: ProviderKind::Anthropic,
        base_url: "https://ai-gateway.vercel.sh",
        auth: AuthMode::ApiKey,
        api_key_env: Some("AI_GATEWAY_API_KEY"),
        // Vercel AI Gateway expects the Anthropic-native `x-api-key` header.
        api_key_header: Some(ApiKeyHeader::XApiKey),
        example_model: Some("anthropic/claude-opus-4.8"),
        note: None,
    },
];

/// Resolve a provider name (case-insensitive) against the catalog, including
/// aliases.
pub fn find(name: &str) -> Option<&'static CatalogEntry> {
    let name = name.trim().to_ascii_lowercase();
    CATALOG
        .iter()
        .find(|entry| entry.name == name || entry.aliases.iter().any(|alias| *alias == name))
}

fn wire_kind(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Responses => "responses",
    }
}

fn wire_auth(auth: AuthMode) -> &'static str {
    match auth {
        AuthMode::Passthrough => "passthrough",
        AuthMode::ApiKey => "api_key",
        AuthMode::ChatgptOauth => "chatgpt_oauth",
        AuthMode::XaiOauth => "xai_oauth",
    }
}

fn wire_header(header: ApiKeyHeader) -> &'static str {
    match header {
        ApiKeyHeader::Bearer => "bearer",
        ApiKeyHeader::XApiKey => "x_api_key",
    }
}

/// Render a paste-ready `[providers.<name>]` block. The whole string is valid
/// TOML — every hint is a `#` comment — so it can be appended to a config file
/// verbatim.
pub fn render(entry: &CatalogEntry) -> String {
    let name = entry.name;
    let mut out = String::new();
    writeln!(out, "[providers.{name}]").unwrap();
    writeln!(out, "kind = \"{}\"", wire_kind(entry.kind)).unwrap();
    writeln!(out, "base_url = \"{}\"", entry.base_url).unwrap();
    writeln!(out, "auth = \"{}\"", wire_auth(entry.auth)).unwrap();
    if let Some(env) = entry.api_key_env {
        writeln!(out, "api_key_env = \"{env}\"").unwrap();
    }
    if let Some(header) = entry.api_key_header {
        writeln!(out, "api_key_header = \"{}\"", wire_header(header)).unwrap();
    }
    if let Some(note) = entry.note {
        writeln!(out, "\n# {note}").unwrap();
    }
    if let Some(env) = entry.api_key_env {
        writeln!(out, "\n# Set the API key in your environment:").unwrap();
        writeln!(out, "#   export {env}=...").unwrap();
    }
    let model = entry.example_model.unwrap_or("<model-id>");
    writeln!(out, "\n# Example route — map a model ID to this provider:").unwrap();
    writeln!(out, "# [[routes]]").unwrap();
    writeln!(out, "# model = \"{model}\"").unwrap();
    writeln!(out, "# provider = \"{name}\"").unwrap();
    out
}

/// Render the list of known providers (for `shunt add provider` with no name),
/// as TOML comments so the output is still safe to redirect into a file.
pub fn list_text() -> String {
    let mut out = String::new();
    writeln!(out, "# Known providers — `shunt add provider <name>`:").unwrap();
    for entry in CATALOG {
        writeln!(out, "#   {:<11} {}", entry.name, entry.base_url).unwrap();
    }
    out
}

/// Handle `shunt add provider [name]`: print a block for a known provider, or
/// the catalog listing when no name is given. An unknown name is an error that
/// names the known providers.
pub fn add_provider(name: Option<&str>) -> anyhow::Result<()> {
    match name {
        None => {
            print!("{}", list_text());
            Ok(())
        }
        Some(name) => match find(name) {
            Some(entry) => {
                print!("{}", render(entry));
                Ok(())
            }
            None => {
                let known = CATALOG
                    .iter()
                    .map(|entry| entry.name)
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::bail!("unknown provider '{name}'. Known providers: {known}")
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use figment::{
        providers::{Format, Serialized, Toml},
        Figment,
    };

    #[test]
    fn find_resolves_names_and_aliases_case_insensitively() {
        assert_eq!(find("openai").unwrap().name, "openai");
        assert_eq!(find("OpenAI").unwrap().name, "openai");
        // Aliases resolve to the canonical entry.
        assert_eq!(find("zai").unwrap().name, "glm");
        assert_eq!(find("z.ai").unwrap().name, "glm");
        assert_eq!(find("moonshot").unwrap().name, "kimi");
        assert_eq!(find("chatgpt").unwrap().name, "codex");
        assert!(find("nope").is_none());
    }

    #[test]
    fn render_openai_block_has_expected_fields() {
        let block = render(find("openai").unwrap());
        assert!(block.contains("[providers.openai]"));
        assert!(block.contains("kind = \"responses\""));
        assert!(block.contains("base_url = \"https://api.openai.com/v1\""));
        assert!(block.contains("auth = \"api_key\""));
        assert!(block.contains("api_key_env = \"OPENAI_API_KEY\""));
        assert!(block.contains("export OPENAI_API_KEY=..."));
        // The example route is commented so the block stays paste-safe.
        assert!(block.contains("# [[routes]]"));
        assert!(block.contains("# provider = \"openai\""));
    }

    #[test]
    fn render_vercel_emits_non_default_api_key_header() {
        let block = render(find("vercel").unwrap());
        assert!(block.contains("api_key_header = \"x_api_key\""));
    }

    #[test]
    fn render_omits_api_key_lines_for_oauth_and_passthrough() {
        // OAuth/passthrough entries have no key to set, so no api_key_env line.
        for name in ["anthropic", "codex", "grok"] {
            let block = render(find(name).unwrap());
            assert!(
                !block.contains("api_key_env"),
                "{name} should not render api_key_env"
            );
        }
    }

    #[test]
    fn list_text_names_every_catalog_entry() {
        let list = list_text();
        for entry in CATALOG {
            assert!(list.contains(entry.name), "list missing {}", entry.name);
        }
    }

    #[test]
    fn add_provider_rejects_unknown_name() {
        let error = add_provider(Some("does-not-exist")).unwrap_err();
        assert!(error.to_string().contains("unknown provider"));
        // The error lists real providers so the user can recover.
        assert!(error.to_string().contains("openai"));
    }

    #[test]
    fn render_pins_literal_auth_token_per_mode() {
        // The round-trip test below only proves each block parses and passes
        // Config::validate(), which adds no semantic check for Passthrough or
        // ChatgptOauth — so a wire_auth swap between them would ship silently.
        // Pin the literal token: codex rendered as `passthrough` would forward
        // the caller's own Anthropic credential to the ChatGPT backend.
        assert!(render(find("anthropic").unwrap()).contains("auth = \"passthrough\""));
        assert!(render(find("codex").unwrap()).contains("auth = \"chatgpt_oauth\""));
        assert!(render(find("grok").unwrap()).contains("auth = \"xai_oauth\""));
    }

    #[test]
    fn catalog_names_and_aliases_are_unique() {
        // find() returns the first match, so a duplicate name/alias would
        // silently shadow an earlier entry — guard every canonical name and
        // alias being pairwise distinct.
        let mut seen = std::collections::HashSet::new();
        for entry in CATALOG {
            assert!(
                seen.insert(entry.name),
                "duplicate catalog key: {}",
                entry.name
            );
            for alias in entry.aliases {
                assert!(seen.insert(*alias), "duplicate catalog key: {alias}");
            }
        }
    }

    /// Every rendered block must parse and pass the real config validation, so
    /// the catalog can't ship a base_url/auth/kind combination the loader would
    /// reject (e.g. an api_key provider with no env, or an xai_oauth host that
    /// trips the bearer-leak guard).
    #[test]
    fn catalog_entries_render_to_valid_config() {
        for entry in CATALOG {
            let block = render(entry);
            let config: Config = Figment::from(Serialized::defaults(Config::default()))
                .merge(Toml::string(&block))
                .extract()
                .unwrap_or_else(|error| panic!("{} block failed to parse: {error}", entry.name));
            config
                .validate()
                .unwrap_or_else(|error| panic!("{} block failed validation: {error}", entry.name));
        }
    }
}
