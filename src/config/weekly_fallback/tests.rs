use std::collections::BTreeMap;

use figment::{
    providers::{Format, Serialized, Toml},
    Figment,
};

use super::{WeeklyFallbackConfig, WeeklyFallbackModel};
use crate::{
    config::{AuthMode, Config, ConfigError, ModelConfig, ProviderKind},
    routing::{resolve_model_chain, resolve_request_chain},
};

const LEGACY: &str = r#"
[server]
default_provider = "claude-main"
[providers.claude-main]
kind = "anthropic"
base_url = "https://api.anthropic.com"
auth = "claude_oauth"
[providers.codex-main]
kind = "responses"
base_url = "https://chatgpt.com/backend-api"
auth = "chatgpt_oauth"
"#;

const ORDERED: &str = r#"
[server]
default_provider = "claude-main"
[[upstreams]]
name = "codex-main"
provider = "codex"
auth = { mode = "chatgpt_oauth", account = "codex-account" }
[[upstreams]]
name = "claude-main"
provider = "anthropic"
auth = { mode = "claude_oauth", account = "claude-account" }
"#;

const POLICY: &str = r#"
[server.weekly_fallback]
enabled = true
claude_provider = "claude-main"
codex_provider = "codex-main"
[[server.weekly_fallback.models]]
claude = "claude-fable-5"
codex = "gpt-6-astra"
claude_fallback = "claude-opus-5"
[[server.weekly_fallback.models]]
claude = "claude-opus-5"
codex = "gpt-5.6-sol"
[[server.weekly_fallback.models]]
claude = "claude-sonnet-5"
codex = "gpt-5.6-terra"
[[server.weekly_fallback.models]]
claude = "claude-haiku-4-5-20251001"
codex = "gpt-5.6-luna"
"#;

fn parse(raw: &str) -> Config {
    Figment::from(Serialized::defaults(Config::default()))
        .merge(Toml::string(raw))
        .extract()
        .unwrap()
}

fn configured(declarations: &str) -> Config {
    parse(&format!("{declarations}\n{POLICY}"))
}

fn policy(config: &mut Config) -> &mut WeeklyFallbackConfig {
    config.server.weekly_fallback.as_mut().unwrap()
}

fn assert_invalid(config: Config, expected: &str) {
    let error = config.validate().unwrap_err();
    let ConfigError::InvalidWeeklyFallback { message } = error else {
        panic!("expected weekly fallback error, got {error}");
    };
    assert!(message.contains(expected), "{message}");
}

#[test]
fn absent_policy_stays_absent_after_serialization() {
    let config = Config::default().validate().unwrap();
    assert!(config.server.weekly_fallback.is_none());
    let serialized = serde_json::to_value(&config).unwrap();
    assert!(serialized["server"].get("weekly_fallback").is_none());
    let decoded: Config = serde_json::from_value(serialized).unwrap();
    assert!(decoded.server.weekly_fallback.is_none());
}

#[test]
fn empty_table_defaults_to_disabled_without_bindings() {
    for raw in [
        "[server.weekly_fallback]",
        "[server.weekly_fallback]\nenabled = false",
    ] {
        let config = parse(raw).validate().unwrap();
        let policy = config.server.weekly_fallback.unwrap();
        assert!(!policy.enabled);
        assert!(policy.claude_provider.is_empty());
        assert!(policy.codex_provider.is_empty());
        assert!(policy.models.is_empty());
    }
}

#[test]
fn disabled_policy_ignores_semantically_invalid_bindings_and_models() {
    let mut config = Config::default();
    config.server.weekly_fallback = Some(WeeklyFallbackConfig {
        enabled: false,
        claude_provider: "missing".into(),
        codex_provider: String::new(),
        models: vec![WeeklyFallbackModel {
            claude: "model[1m]".into(),
            codex: String::new(),
            claude_fallback: Some("model[1m]".into()),
        }],
    });
    config.validate().unwrap();
}

#[test]
fn both_declaration_forms_validate_explicit_pairs_after_normalization() {
    for declarations in [LEGACY, ORDERED] {
        let config = configured(declarations).validate().unwrap();
        let policy = config.server.weekly_fallback.as_ref().unwrap();
        assert!(policy.enabled);
        assert_eq!(policy.claude_provider, "claude-main");
        assert_eq!(policy.codex_provider, "codex-main");
        assert_eq!(config.providers["claude-main"].auth, AuthMode::ClaudeOauth);
        assert_eq!(config.providers["codex-main"].auth, AuthMode::ChatgptOauth);
        let expected = [
            ("claude-fable-5", "gpt-6-astra", Some("claude-opus-5")),
            ("claude-opus-5", "gpt-5.6-sol", None),
            ("claude-sonnet-5", "gpt-5.6-terra", None),
            ("claude-haiku-4-5-20251001", "gpt-5.6-luna", None),
        ];
        let actual = policy
            .models
            .iter()
            .map(|pair| {
                (
                    pair.claude.as_str(),
                    pair.codex.as_str(),
                    pair.claude_fallback.as_deref(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        let serialized = serde_json::to_value(policy).unwrap();
        assert!(serialized["models"][1].get("claude_fallback").is_none());
        if declarations == ORDERED {
            assert_eq!(config.upstream_order, ["codex-main", "claude-main"]);
            assert_eq!(
                config.providers["claude-main"].account_scope,
                ["claude-account"]
            );
            assert_eq!(
                config.providers["codex-main"].account_scope,
                ["codex-account"]
            );
        }
    }
}

#[test]
fn enabled_policy_requires_known_nonempty_bindings_in_both_forms() {
    for declarations in [LEGACY, ORDERED] {
        for field in ["claude_provider", "codex_provider"] {
            for name in ["", " \t", "missing"] {
                let mut config = configured(declarations);
                let policy = policy(&mut config);
                match field {
                    "claude_provider" => policy.claude_provider = name.into(),
                    _ => policy.codex_provider = name.into(),
                }
                assert_invalid(config, field);
            }
        }
    }
}

#[test]
fn enabled_policy_requires_models() {
    let mut config = configured(LEGACY);
    policy(&mut config).models.clear();
    assert_invalid(config, "at least one backend model pair");
}

#[test]
fn ordered_bindings_cannot_reference_replaced_builtin_providers() {
    for field in ["claude_provider", "codex_provider"] {
        let mut config = configured(ORDERED);
        let policy = policy(&mut config);
        match field {
            "claude_provider" => policy.claude_provider = "anthropic".into(),
            _ => policy.codex_provider = "codex".into(),
        }
        assert_invalid(config, "unknown provider");
    }
}

#[test]
fn duplicate_backend_mappings_are_invalid_on_either_provider() {
    for field in ["claude", "codex"] {
        let mut config = configured(LEGACY);
        let models = &mut policy(&mut config).models;
        match field {
            "claude" => models[1].claude = models[0].claude.clone(),
            _ => models[1].codex = models[0].codex.clone(),
        }
        assert_invalid(config, &format!("duplicate {field} model"));
    }
}

#[test]
fn fallback_cannot_target_its_primary_model() {
    let mut config = configured(LEGACY);
    let pair = &mut policy(&mut config).models[0];
    pair.claude_fallback = Some(pair.claude.clone());
    assert_invalid(config, "claude_fallback must differ from claude");
}

#[test]
fn every_backend_id_rejects_invalid_values_and_context_hints() {
    for field in ["claude", "codex", "claude_fallback"] {
        for model in [
            "",
            " \t",
            "model name",
            "model\0name",
            "model[1m]",
            "model[1M]",
        ] {
            let mut config = configured(LEGACY);
            let pair = &mut policy(&mut config).models[0];
            match field {
                "claude" => pair.claude = model.into(),
                "codex" => pair.codex = model.into(),
                _ => pair.claude_fallback = Some(model.into()),
            }
            assert_invalid(config, &format!("models[0].{field}"));
        }
    }
}

#[test]
fn backend_ids_require_no_family_name_heuristic() {
    let mut config = configured(LEGACY);
    policy(&mut config).models = vec![WeeklyFallbackModel {
        claude: "operator-primary-v20260915".into(),
        codex: "operator-alternate-v20260915".into(),
        claude_fallback: Some("operator-fallback-v20260915".into()),
    }];
    config.validate().unwrap();
}

#[test]
fn incompatible_auth_and_kinds_are_invalid_for_either_binding() {
    for (field, name, wrong_kind) in [
        ("claude_provider", "claude-main", ProviderKind::Responses),
        ("codex_provider", "codex-main", ProviderKind::Anthropic),
    ] {
        for auth in [AuthMode::Passthrough, AuthMode::ApiKey, AuthMode::KimiOauth] {
            let mut config = configured(LEGACY);
            config.providers.get_mut(name).unwrap().auth = auth;
            assert_invalid(config, field);
        }
        let mut config = configured(LEGACY);
        config.providers.get_mut(name).unwrap().kind = wrong_kind;
        assert_invalid(config, field);
    }
}

#[test]
fn ordered_kimi_provider_cannot_fill_either_binding() {
    let kimi = "[[upstreams]]\nname = \"kimi-main\"\nprovider = \"kimi\"\nauth = \"kimi_oauth\"";
    for field in ["claude_provider", "codex_provider"] {
        let mut config = configured(&format!("{ORDERED}\n{kimi}"));
        let policy = policy(&mut config);
        match field {
            "claude_provider" => policy.claude_provider = "kimi-main".into(),
            _ => policy.codex_provider = "kimi-main".into(),
        }
        assert_invalid(config, field);
    }
}

#[test]
fn codex_binding_uses_existing_backend_origin_validation() {
    let mut config = configured(LEGACY);
    config.providers.get_mut("codex-main").unwrap().base_url = "https://api.openai.com/v1".into();
    assert!(matches!(
        config.validate().unwrap_err(),
        ConfigError::ChatgptOauthNonChatgptHost { provider, .. } if provider == "codex-main"
    ));
    let mut config = configured(LEGACY);
    config.providers.get_mut("codex-main").unwrap().base_url = "http://127.0.0.1:12345".into();
    config.validate().unwrap();
}

fn chain_config(first: &str, enabled: bool) -> Config {
    let mut config = configured(&format!(
        "{ORDERED}\n[[upstreams]]\nname = \"other\"\nprovider = \"openai\"\n\
         [[upstreams]]\nname = \"third\"\nprovider = \"openai\""
    ));
    policy(&mut config).enabled = enabled;
    config.models = vec![ModelConfig {
        id: "chain-alias".into(),
        display_name: None,
        upstream_model: Some(BTreeMap::from([
            (first.into(), "primary-backend".into()),
            ("other".into(), "alternate-backend".into()),
        ])),
        stage_router: None,
    }];
    config
}

#[test]
fn generic_chains_cannot_use_either_bound_provider() {
    for provider in ["claude-main", "codex-main"] {
        assert_invalid(chain_config(provider, true), "generic route chain");
        let config = chain_config(provider, false).validate().unwrap();
        let routes = resolve_model_chain(&config, "chain-alias[1M]");
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].provider, provider);
        assert_eq!(routes[1].provider, "other");
    }
}

#[test]
fn unrelated_generic_chains_keep_their_existing_routes() {
    let config = chain_config("third", true).validate().unwrap();
    let routes = resolve_model_chain(&config, "chain-alias");
    assert_eq!(routes.len(), 2);
    assert_eq!(routes[0].provider, "other");
    assert_eq!(routes[1].provider, "third");
}

#[test]
fn aliases_and_request_context_hints_resolve_to_exact_backend_ids() {
    for declarations in [LEGACY, ORDERED] {
        for (provider, backend) in [
            ("claude-main", "claude-fable-5"),
            ("codex-main", "gpt-6-astra"),
        ] {
            let mut config = configured(declarations);
            config.models = vec![ModelConfig {
                id: "operator-alias".into(),
                display_name: None,
                upstream_model: Some(BTreeMap::from([(provider.into(), backend.into())])),
                stage_router: None,
            }];
            let config = config.validate().unwrap();
            for requested in ["operator-alias", "operator-alias[1m]", "operator-alias[1M]"] {
                let body = serde_json::to_vec(&serde_json::json!({ "model": requested })).unwrap();
                let (routes, original) = resolve_request_chain(&config, &body).unwrap();
                assert_eq!(original, requested);
                assert_eq!(routes.len(), 1);
                assert_eq!(routes[0].provider, provider);
                assert_eq!(routes[0].upstream_model, backend);
                assert_eq!(routes[0].model, "operator-alias");
            }
        }
    }
}
