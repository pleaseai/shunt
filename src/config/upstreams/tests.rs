use figment::{
    providers::{Format, Toml},
    Figment,
};

use super::{normalize, UpstreamConfig};
use crate::config::{ApiKeyHeader, AuthMode, ConfigError, ProviderKind};

fn parse(raw: &str) -> UpstreamConfig {
    Figment::from(Toml::string(raw)).extract().unwrap()
}

#[test]
fn declaration_order_and_explicit_preset_overrides_are_preserved() {
    let upstreams = vec![
        parse("name = \"second\"\nprovider = \"codex\"\neffort = \"high\""),
        parse(
            "name = \"first\"\nprovider = \"openai\"\nkind = \"anthropic\"\nbase_url = \"https://gateway.example\"\nauth = \"passthrough\"",
        ),
    ];
    let (providers, order) = normalize(&upstreams).unwrap();
    assert_eq!(order, ["second", "first"]);
    assert_eq!(providers["second"].auth, AuthMode::ChatgptOauth);
    assert_eq!(providers["second"].effort.as_deref(), Some("high"));
    assert_eq!(providers["first"].kind, ProviderKind::Anthropic);
    assert_eq!(providers["first"].base_url, "https://gateway.example");
    assert_eq!(providers["first"].auth, AuthMode::Passthrough);
}

#[test]
fn auth_string_and_map_shorthand_normalize_identically() {
    let string = parse(
        "name = \"a\"\nkind = \"anthropic\"\nbase_url = \"https://api.anthropic.com\"\nauth = \"claude_oauth\"",
    );
    let map = parse(
        "name = \"a\"\nkind = \"anthropic\"\nbase_url = \"https://api.anthropic.com\"\nauth = { mode = \"claude_oauth\" }",
    );
    let (string, _) = normalize(&[string]).unwrap();
    let (map, _) = normalize(&[map]).unwrap();
    assert_eq!(string["a"].auth, map["a"].auth);
    assert_eq!(string["a"].account_scope, map["a"].account_scope);
    assert_eq!(string["a"].accounts.len(), map["a"].accounts.len());
}

#[test]
fn api_key_map_absorbs_env_and_header() {
    let upstream = parse(
        "name = \"custom\"\nkind = \"responses\"\nbase_url = \"https://api.example\"\nauth = { mode = \"api_key\", env = \"CUSTOM_KEY\", header = \"x_api_key\" }",
    );
    let (providers, _) = normalize(&[upstream]).unwrap();
    assert_eq!(providers["custom"].auth, AuthMode::ApiKey);
    assert_eq!(
        providers["custom"].api_key_env.as_deref(),
        Some("CUSTOM_KEY")
    );
    assert_eq!(providers["custom"].api_key_header, ApiKeyHeader::XApiKey);
}

#[test]
fn oauth_scope_separates_store_references_and_inline_accounts() {
    let upstream = parse(
        r#"name = "claude"
provider = "anthropic"
auth = { mode = "claude_oauth", accounts = ["stored", { name = "inline", token_env = "TOKEN" }] }
"#,
    );
    let (providers, _) = normalize(&[upstream]).unwrap();
    assert_eq!(providers["claude"].account_scope, ["stored"]);
    assert_eq!(providers["claude"].accounts[0].name, "inline");
}

#[test]
fn strict_auth_map_and_upstream_fields_reject_typos() {
    for raw in [
        "name = \"x\"\nprovider = \"openai\"\nauth = { mode = \"api_key\", typo = \"x\" }",
        "name = \"x\"\nprovider = \"anthropic\"\nauth = { mode = \"claude_oauth\", accounts = [{ name = \"inline\", token_envv = \"TOKEN\" }] }",
        "name = \"x\"\nprovider = \"openai\"\ntypo = true",
    ] {
        let error = Figment::from(Toml::string(raw))
            .extract::<UpstreamConfig>()
            .unwrap_err();
        assert!(!error.to_string().is_empty());
    }
}

#[test]
fn explicitly_empty_accounts_are_rejected() {
    let upstream = parse(
        "name = \"a\"\nprovider = \"anthropic\"\nauth = { mode = \"claude_oauth\", accounts = [] }",
    );
    assert!(matches!(
        normalize(&[upstream]).unwrap_err(),
        ConfigError::EmptyUpstreamAccountList { upstream } if upstream == "a"
    ));
}

#[test]
fn account_and_accounts_are_mutually_exclusive() {
    let upstream = parse(
        "name = \"a\"\nprovider = \"anthropic\"\nauth = { mode = \"claude_oauth\", account = \"one\", accounts = [\"two\"] }",
    );
    assert!(matches!(
        normalize(&[upstream]).unwrap_err(),
        ConfigError::UpstreamAuthAccountConflict { .. }
    ));
}

static CONFIG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct TempConfig(std::path::PathBuf);

impl TempConfig {
    fn new(raw: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "shunt-upstreams-test-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, raw).unwrap();
        Self(path)
    }
}

fn load(file: &TempConfig) -> Result<crate::config::Config, ConfigError> {
    let _guard = CONFIG_ENV_LOCK.lock().unwrap();
    crate::config::Config::load(Some(&file.0))
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn load_replaces_builtin_providers_and_preserves_order() {
    let file = TempConfig::new(
        r#"
[server]
default_provider = "primary"

[[upstreams]]
name = "primary"
provider = "openai"
effort = "high"

[[upstreams]]
name = "fallback"
provider = "kimi"

[[models]]
id = "alias"
[models.upstream_model]
fallback = "kimi-k2"
primary = "gpt-5.6-sol"
"#,
    );

    let config = load(&file).unwrap();
    assert!(config.upstreams_ordered);
    assert_eq!(config.upstream_order, ["primary", "fallback"]);
    assert_eq!(config.providers.len(), 2);
    assert!(config.provider("anthropic").is_none());
    assert_eq!(
        config.provider("primary").unwrap().effort.as_deref(),
        Some("high")
    );
    let kimi = config.provider("fallback").unwrap();
    assert_eq!(kimi.base_url, "https://api.moonshot.ai/anthropic");
    assert_eq!(kimi.api_key_env.as_deref(), Some("MOONSHOT_API_KEY"));
}

#[test]
fn load_rejects_unknown_preset_and_lists_available_presets() {
    let file = TempConfig::new(
        r#"
[server]
default_provider = "bad"
[[upstreams]]
name = "bad"
provider = "missing"
"#,
    );
    let error = load(&file).unwrap_err();
    assert!(matches!(error, ConfigError::UnknownProviderPreset { .. }));
    let message = error.to_string();
    assert!(message.contains("available presets:"));
    assert!(message.contains("anthropic"));
    assert!(message.contains("kimi"));
    assert!(message.contains("cursor"));
}

#[test]
fn load_rejects_duplicate_empty_and_whitespace_names() {
    for raw in [
        "[[upstreams]]\nname = \"same\"\nprovider = \"anthropic\"\n[[upstreams]]\nname = \"same\"\nprovider = \"anthropic\"",
        "[[upstreams]]\nname = \"\"\nprovider = \"anthropic\"",
        "[[upstreams]]\nname = \"  \\t\"\nprovider = \"anthropic\"",
    ] {
        let file = TempConfig::new(raw);
        let error = load(&file).unwrap_err();
        assert!(matches!(
            error,
            ConfigError::DuplicateUpstreamName { .. } | ConfigError::EmptyUpstreamName { .. }
        ));
    }
}

#[test]
fn manual_upstream_requires_kind_base_url_and_api_key_env() {
    for (raw, expected) in [
        (
            "[[upstreams]]\nname = \"manual\"\nbase_url = \"https://api.example\"",
            "kind is required",
        ),
        (
            "[[upstreams]]\nname = \"manual\"\nkind = \"responses\"",
            "base_url is required",
        ),
        (
            "[[upstreams]]\nname = \"manual\"\nkind = \"responses\"\nbase_url = \"https://api.example\"\nauth = \"api_key\"",
            "env is not set",
        ),
    ] {
        let file = TempConfig::new(raw);
        assert!(
            load(&file)
                .unwrap_err()
                .to_string()
                .contains(expected)
        );
    }
}

#[test]
fn load_rejects_both_provider_declaration_forms() {
    let file = TempConfig::new(
        r#"
[[upstreams]]
name = "anthropic"
provider = "anthropic"
[providers.custom]
kind = "anthropic"
base_url = "https://api.example"
"#,
    );
    assert!(matches!(
        load(&file).unwrap_err(),
        ConfigError::MixedProviderDeclarationForms
    ));
}

#[test]
fn ordered_upstreams_keep_the_default_provider_namespace_strict() {
    let file = TempConfig::new("[[upstreams]]\nname = \"only-custom\"\nprovider = \"openai\"");
    assert!(matches!(
        load(&file).unwrap_err(),
        ConfigError::UnknownDefaultProvider(provider) if provider == "anthropic"
    ));
}

#[test]
fn ordered_multi_provider_model_maps_validate() {
    let file = TempConfig::new(
        r#"
[server]
default_provider = "first"
[[upstreams]]
name = "first"
provider = "openai"
[[upstreams]]
name = "second"
provider = "codex"
[[models]]
id = "alias"
[models.upstream_model]
second = "gpt-codex"
first = "gpt-openai"
"#,
    );
    let config = load(&file).unwrap();
    assert_eq!(config.models[0].upstream_model.as_ref().unwrap().len(), 2);
}

#[test]
fn ordered_provider_env_override_addresses_declared_name() {
    let file = TempConfig::new(
        "[server]\ndefault_provider = \"primary\"\n[[upstreams]]\nname = \"primary\"\nprovider = \"openai\"",
    );
    let env = "SHUNT_PROVIDERS__PRIMARY__EFFORT";
    let _guard = CONFIG_ENV_LOCK.lock().unwrap();
    std::env::set_var(env, "high");
    let result = crate::config::Config::load(Some(&file.0));
    std::env::remove_var(env);
    assert_eq!(
        result
            .unwrap()
            .provider("primary")
            .unwrap()
            .effort
            .as_deref(),
        Some("high")
    );
}

#[test]
fn ordered_provider_env_override_cannot_declare_a_name() {
    let file = TempConfig::new(
        "[server]\ndefault_provider = \"primary\"\n[[upstreams]]\nname = \"primary\"\nprovider = \"openai\"",
    );
    let env = "SHUNT_PROVIDERS__MISSING__EFFORT";
    let _guard = CONFIG_ENV_LOCK.lock().unwrap();
    std::env::set_var(env, "high");
    let result = crate::config::Config::load(Some(&file.0));
    std::env::remove_var(env);
    assert!(matches!(
        result.unwrap_err(),
        ConfigError::UnknownUpstreamEnvOverride { upstream } if upstream == "missing"
    ));
}

#[test]
fn legacy_provider_env_override_behavior_is_unchanged() {
    let file = TempConfig::new("[server]\ndefault_provider = \"environmental\"");
    let env = "SHUNT_PROVIDERS__ENVIRONMENTAL";
    let _guard = CONFIG_ENV_LOCK.lock().unwrap();
    std::env::set_var(
        env,
        "{kind=\"anthropic\",base_url=\"https://api.example\",auth=\"passthrough\"}",
    );
    let result = crate::config::Config::load(Some(&file.0));
    std::env::remove_var(env);
    let config = result.unwrap();
    assert!(!config.upstreams_ordered);
    assert_eq!(
        config.provider("environmental").unwrap().base_url,
        "https://api.example"
    );
    assert!(config
        .upstream_order
        .windows(2)
        .all(|pair| pair[0] < pair[1]));
}
