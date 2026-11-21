//! Exchange token behaviour. Revocation is exercised through the real reload
//! path (`shared.store` + `AppState::refreshed`), because the property under
//! test is "a config change reaches tokens already issued".

use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

use super::{fingerprint, is_exchange_shaped, mint, verify, ExchangeSettings, EXCHANGE_ISSUER};
use crate::auth::{hs256, inbound::InboundAuth, inbound_jwt::JwksCache};
use crate::config::{
    Config, ExchangeConfig, InboundAuthConfig, InboundJwtConfig, Secret, StringList,
};
use crate::gateway::approval::Identity;

const SECRET_A: &str = "exchange-secret-0123456789abcdef0123";
const SECRET_B: &str = "exchange-secret-fedcba9876543210fedc";
const NOW: u64 = 1_000_000;

fn entry(issuer: &str, exchange: bool) -> InboundJwtConfig {
    InboundJwtConfig {
        issuer: issuer.to_string(),
        audience: StringList::One("shunt-aud".to_string()),
        email_domains: Vec::new(),
        allowed_emails: Vec::new(),
        require: [(
            "repository".to_string(),
            StringList::Many(vec!["org/a".to_string(), "org/b".to_string()]),
        )]
        .into(),
        subject_prefix: StringList::default(),
        identity_claim: Some("repository".to_string()),
        algorithms: vec!["RS256".to_string()],
        authorized_parties: Vec::new(),
        clock_skew_seconds: 0,
        max_token_age_seconds: 3600,
        jwks_url: Some("http://127.0.0.1:9/jwks".to_string()),
        exchange,
    }
}

fn exchange_table(secrets: &[&str]) -> ExchangeConfig {
    ExchangeConfig {
        signing_secret: secrets.iter().map(|s| Secret::from(*s)).collect(),
        max_ttl_seconds: 600,
    }
}

fn config_with(env: &str, entries: Vec<InboundJwtConfig>, secrets: &[&str]) -> Config {
    let mut config = Config::default();
    config.server.auth = Some(InboundAuthConfig {
        tokens_env: env.to_string(),
        jwt: entries,
        exchange: Some(exchange_table(secrets)),
        ..Default::default()
    });
    config
}

/// A running config the test can reload, plus the env guard that keeps
/// `tokens_env` unset for its lifetime.
struct Harness {
    shared: crate::reload::SharedState,
    state: crate::server::AppState,
    env: String,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Harness {
    fn new(tag: &str, entries: Vec<InboundJwtConfig>, secrets: &[&str]) -> Self {
        let lock = crate::config::test_env_lock();
        let env = format!("SHUNT_TEST_EXCHANGE_NO_TOKENS_{tag}_{}", std::process::id());
        std::env::remove_var(&env);
        let shared: crate::reload::SharedState = Arc::new(arc_swap::ArcSwap::from_pointee(
            crate::reload::RuntimeState::from_config(config_with(&env, entries, secrets)).unwrap(),
        ));
        let state = crate::server::AppState::from_shared(
            shared.clone(),
            reqwest::Client::new(),
            Arc::new(crate::accounts::AccountPool::new()),
            Arc::new(crate::upstream_status::StatusStore::new()),
            Arc::new(crate::admin::AdminStores::new()),
            Arc::new(crate::gateway::GatewayStores::default()),
            Arc::new(crate::routing::stage::StageRouterStore::new()),
            Arc::new(JwksCache::new()),
            true,
        );
        Self {
            shared,
            state,
            env,
            _lock: lock,
        }
    }

    fn auth(&self) -> &InboundAuth {
        self.state.inbound_auth.as_deref().expect("auth configured")
    }

    /// Mint as the `index`th configured entry would after verifying an assertion.
    fn mint_from(&self, index: usize, identity: &str) -> String {
        let auth = self.auth();
        mint(
            auth.exchange().expect("exchange configured"),
            identity,
            &fingerprint(&auth.jwt()[index]),
            NOW,
        )
        .token
    }

    /// Swap in `entries`/`secrets` through the real reload path and return
    /// the refreshed snapshot's verdict on `token`.
    fn reload_then_verify(
        &self,
        entries: Vec<InboundJwtConfig>,
        secrets: &[&str],
        token: &str,
    ) -> Option<String> {
        self.shared.store(Arc::new(
            crate::reload::RuntimeState::from_config(config_with(&self.env, entries, secrets))
                .unwrap(),
        ));
        let reloaded = self.state.refreshed();
        verify(
            reloaded.inbound_auth.as_deref().expect("auth configured"),
            token,
            NOW + 1,
        )
    }
}

#[test]
fn token_verifies_until_exp_and_not_after() {
    let h = Harness::new("TTL", vec![entry("https://idp.example", true)], &[SECRET_A]);
    let token = h.mint_from(0, "org/a");
    assert_eq!(verify(h.auth(), &token, NOW + 1).as_deref(), Some("org/a"));
    assert_eq!(
        verify(h.auth(), &token, NOW + 599).as_deref(),
        Some("org/a")
    );
    assert_eq!(
        verify(h.auth(), &token, NOW + 600),
        None,
        "exp is exclusive"
    );
    assert_eq!(verify(h.auth(), &token, NOW + 10_000), None);
}

#[test]
fn mint_reports_the_configured_lifetime() {
    let h = Harness::new(
        "LIFE",
        vec![entry("https://idp.example", true)],
        &[SECRET_A],
    );
    let auth = h.auth();
    let minted = mint(auth.exchange().unwrap(), "x", "fp", NOW);
    assert_eq!(minted.expires_in, 600);
}

#[test]
fn unchanged_reload_with_reordered_rules_still_verifies() {
    let toml_a = r#"
        tokens_env = "SHUNT_TEST_EXCHANGE_REORDER_A"
        [[jwt]]
        issuer = "https://idp.example"
        audience = ["a1", "a2"]
        jwks_url = "http://127.0.0.1:9/jwks"
        identity_claim = "repository"
        exchange = true
        [jwt.require]
        repository = ["org/a", "org/b"]
        ref = ["refs/heads/main", "refs/heads/dev"]
    "#;
    let toml_b = r#"
        tokens_env = "SHUNT_TEST_EXCHANGE_REORDER_A"
        [[jwt]]
        issuer = "https://idp.example"
        audience = ["a2", "a1"]
        jwks_url = "http://127.0.0.1:9/jwks"
        identity_claim = "repository"
        exchange = true
        [jwt.require]
        ref = ["refs/heads/dev", "refs/heads/main"]
        repository = ["org/b", "org/a"]
    "#;
    let table = || Some(exchange_table(&[SECRET_A]));
    let parse = |text: &str| {
        let mut parsed: InboundAuthConfig = toml::from_str(text).expect("toml parses");
        parsed.exchange = table();
        parsed
    };
    let reorder = |text: &str| {
        let mut config = Config::default();
        config.server.auth = Some(parse(text));
        config
    };
    let _lock = crate::config::test_env_lock();
    let a = reorder(toml_a).server.auth.unwrap().resolve().unwrap();
    let b = reorder(toml_b).server.auth.unwrap().resolve().unwrap();
    let token = mint(
        a.exchange().unwrap(),
        "org/a",
        &fingerprint(&a.jwt()[0]),
        NOW,
    )
    .token;
    assert_eq!(verify(&b, &token, NOW + 1).as_deref(), Some("org/a"));
    assert_eq!(fingerprint(&a.jwt()[0]), fingerprint(&b.jwt()[0]));
}

#[test]
fn removing_the_verifying_entry_revokes() {
    let h = Harness::new(
        "RM",
        vec![
            entry("https://idp.example", true),
            entry("https://other.example", true),
        ],
        &[SECRET_A],
    );
    let token = h.mint_from(0, "org/a");
    assert_eq!(
        h.reload_then_verify(
            vec![entry("https://other.example", true)],
            &[SECRET_A],
            &token
        ),
        None
    );
}

#[test]
fn clearing_exchange_opt_in_revokes() {
    let h = Harness::new(
        "OPT",
        vec![
            entry("https://idp.example", true),
            entry("https://other.example", true),
        ],
        &[SECRET_A],
    );
    let token = h.mint_from(0, "org/a");
    assert_eq!(
        h.reload_then_verify(
            vec![
                entry("https://idp.example", false),
                entry("https://other.example", true)
            ],
            &[SECRET_A],
            &token
        ),
        None
    );
}

#[test]
fn dropping_a_require_value_revokes() {
    // AC-016: the token's repository is removed from the entry's `require`.
    let h = Harness::new("REQ", vec![entry("https://idp.example", true)], &[SECRET_A]);
    let token = h.mint_from(0, "org/b");
    let mut narrowed = entry("https://idp.example", true);
    narrowed.require = [(
        "repository".to_string(),
        StringList::Many(vec!["org/a".to_string()]),
    )]
    .into();
    assert_eq!(
        h.reload_then_verify(vec![narrowed], &[SECRET_A], &token),
        None
    );
}

#[test]
fn every_policy_field_change_revokes_and_tuning_fields_do_not() {
    let base = entry("https://idp.example", true);
    let h = Harness::new("FIELDS", vec![base.clone()], &[SECRET_A]);
    let token = h.mint_from(0, "org/a");
    type Edit = fn(&mut InboundJwtConfig);
    let revoking: [(&str, Edit); 6] = [
        ("audience", |e| e.audience = StringList::One("other".into())),
        ("algorithms", |e| e.algorithms = vec!["ES256".into()]),
        ("authorized_parties", |e| {
            e.authorized_parties = vec!["azp".into()]
        }),
        ("allowed_emails", |e| {
            e.allowed_emails = vec!["a@b.example".into()]
        }),
        ("subject_prefix", |e| {
            e.subject_prefix = StringList::One("repo:org/".into())
        }),
        ("identity_claim", |e| e.identity_claim = Some("sub".into())),
    ];
    for (name, edit) in revoking {
        let mut edited = base.clone();
        edit(&mut edited);
        assert_eq!(
            h.reload_then_verify(vec![edited], &[SECRET_A], &token),
            None,
            "editing {name} must revoke"
        );
    }
    let mut domains = base.clone();
    domains.email_domains = vec!["example.com".into()];
    assert_eq!(
        h.reload_then_verify(vec![domains], &[SECRET_A], &token),
        None
    );

    let mut tuned = base.clone();
    tuned.clock_skew_seconds = 30;
    tuned.max_token_age_seconds = 60;
    tuned.jwks_url = Some("http://127.0.0.1:9/other".to_string());
    assert_eq!(
        h.reload_then_verify(vec![tuned], &[SECRET_A], &token)
            .as_deref(),
        Some("org/a"),
        "freshness and key-location tuning must not revoke"
    );
}

#[test]
fn two_entries_on_one_issuer_bind_to_the_minting_entry() {
    let mut first = entry("https://idp.example", true);
    first.require = [("repository".to_string(), StringList::One("org/a".into()))].into();
    let mut second = entry("https://idp.example", true);
    second.require = [("repository".to_string(), StringList::One("org/b".into()))].into();
    let h = Harness::new("TWO", vec![first.clone(), second.clone()], &[SECRET_A]);
    let token = h.mint_from(1, "org/b");

    let mut first_edited = first.clone();
    first_edited.audience = StringList::One("changed".into());
    assert_eq!(
        h.reload_then_verify(vec![first_edited, second.clone()], &[SECRET_A], &token)
            .as_deref(),
        Some("org/b"),
        "editing entry 1 must not touch entry 2's tokens"
    );

    let mut second_edited = second.clone();
    second_edited.audience = StringList::One("changed".into());
    assert_eq!(
        h.reload_then_verify(vec![first, second_edited], &[SECRET_A], &token),
        None,
        "editing entry 2 must revoke its tokens"
    );
}

#[test]
fn secret_rotation_keeps_non_first_secrets_and_drops_removed_ones() {
    let entries = || vec![entry("https://idp.example", true)];
    let h = Harness::new("ROT", entries(), &[SECRET_A]);
    let token = h.mint_from(0, "org/a");
    assert_eq!(
        h.reload_then_verify(entries(), &[SECRET_B, SECRET_A], &token)
            .as_deref(),
        Some("org/a"),
        "a signing secret rotated to a non-first slot still verifies"
    );
    assert_eq!(
        h.reload_then_verify(entries(), &[SECRET_B], &token),
        None,
        "dropping every secret that signed the token rejects it"
    );
}

#[test]
fn absent_exchange_settings_reject() {
    let h = Harness::new("OFF", vec![entry("https://idp.example", true)], &[SECRET_A]);
    let token = h.mint_from(0, "org/a");
    let bare = InboundAuth::new(
        axum::http::HeaderName::from_static("x-shunt-token"),
        Vec::new(),
    );
    assert_eq!(verify(&bare, &token, NOW + 1), None);
}

fn forged(settings: &ExchangeSettings, claims: serde_json::Value) -> String {
    hs256::sign_token(
        &serde_json::to_vec(&claims).unwrap(),
        settings.signing_secret(),
    )
}

#[test]
fn a_valid_signature_without_the_marker_or_with_wrong_claims_is_rejected() {
    let h = Harness::new(
        "CLAIMS",
        vec![entry("https://idp.example", true)],
        &[SECRET_A],
    );
    let auth = h.auth();
    let settings = auth.exchange().unwrap();
    let ent = fingerprint(&auth.jwt()[0]);
    let good = serde_json::json!({
        "iss": EXCHANGE_ISSUER, "aud": "shunt-exchange", "sub": "org/a",
        "iat": NOW, "exp": NOW + 600, "ent": ent, "shunt_token_use": "workload-exchange",
    });
    assert_eq!(
        verify(auth, &forged(settings, good.clone()), NOW + 1).as_deref(),
        Some("org/a"),
        "the control claims set verifies, so each rejection below is its one edit"
    );
    let edits: [(&str, serde_json::Value); 6] = [
        ("marker absent", {
            let mut v = good.clone();
            v.as_object_mut().unwrap().remove("shunt_token_use");
            v
        }),
        ("marker wrong", {
            let mut v = good.clone();
            v["shunt_token_use"] = "gateway-session".into();
            v
        }),
        ("issuer", {
            let mut v = good.clone();
            v["iss"] = "https://idp.example".into();
            v
        }),
        ("audience", {
            let mut v = good.clone();
            v["aud"] = "shunt".into();
            v
        }),
        ("expired", {
            let mut v = good.clone();
            v["exp"] = (NOW - 1).into();
            v
        }),
        ("unknown entry", {
            let mut v = good.clone();
            v["ent"] = "not-a-fingerprint".into();
            v
        }),
    ];
    for (name, claims) in edits {
        assert_eq!(
            verify(auth, &forged(settings, claims), NOW + 1),
            None,
            "{name} must be rejected"
        );
    }
}

#[test]
fn malformed_input_is_rejected_without_panicking() {
    let h = Harness::new(
        "JUNK",
        vec![entry("https://idp.example", true)],
        &[SECRET_A],
    );
    let good = h.mint_from(0, "org/a");
    let tampered = {
        let mut parts: Vec<&str> = good.split('.').collect();
        parts[2] = "AAAA";
        parts.join(".")
    };
    for junk in [
        "",
        ".",
        "..",
        "a.b",
        "a.b.c.d",
        "not a jwt",
        "\u{1F980}.\u{1F980}.\u{1F980}",
    ] {
        assert_eq!(verify(h.auth(), junk, NOW + 1), None, "{junk:?}");
        assert!(!is_exchange_shaped(junk.as_bytes()), "{junk:?}");
    }
    // A bad signature fails verification but is still shaped like our token.
    assert_eq!(verify(h.auth(), &tampered, NOW + 1), None);
    assert!(is_exchange_shaped(tampered.as_bytes()));
    assert!(!is_exchange_shaped(&[0xff, 0xfe, b'.', b'.']));
}

#[test]
fn shape_predicate_matches_the_marker_regardless_of_validity() {
    let h = Harness::new(
        "SHAPE",
        vec![entry("https://idp.example", true)],
        &[SECRET_A],
    );
    let token = h.mint_from(0, "org/a");
    assert!(is_exchange_shaped(token.as_bytes()));
    let rotated = hs256::sign_token(
        br#"{"shunt_token_use":"workload-exchange","exp":1}"#,
        b"some entirely different secret!!!!!!!!",
    );
    assert!(
        is_exchange_shaped(rotated.as_bytes()),
        "signature and exp are not judged"
    );
    let other = hs256::sign_token(br#"{"shunt_token_use":"gateway-session"}"#, b"k");
    assert!(!is_exchange_shaped(other.as_bytes()));
    let plain = format!(
        "{}.{}.sig",
        URL_SAFE_NO_PAD.encode(hs256::HEADER_JSON),
        URL_SAFE_NO_PAD.encode(br#"{"sub":"x"}"#)
    );
    assert!(!is_exchange_shaped(plain.as_bytes()));
}

#[test]
fn the_two_token_families_reject_each_other_under_one_secret() {
    // Same secret bytes on both sides, so every rejection below comes from
    // the claim checks and not from a signature mismatch.
    let h = Harness::new("FAM", vec![entry("https://idp.example", true)], &[SECRET_A]);
    let identity = Identity {
        sub: "org/a".into(),
        email: "a@example.com".into(),
        name: "a".into(),
    };
    let gateway_token =
        crate::gateway::jwt::mint(&identity, "https://gw.example", SECRET_A.as_bytes(), 600);
    let exchange_token = h.mint_from(0, "org/a");
    // Both tokens are well-formed for their own verifier.
    assert!(
        crate::gateway::jwt::verify(&gateway_token, "https://gw.example", SECRET_A.as_bytes())
            .is_some()
    );
    assert!(verify(h.auth(), &exchange_token, NOW + 1).is_some());
    // And neither is accepted by the other's.
    assert!(
        crate::gateway::jwt::verify(&exchange_token, EXCHANGE_ISSUER, SECRET_A.as_bytes())
            .is_none(),
        "gateway verify must reject an exchange token even under its own issuer"
    );
    assert!(crate::gateway::jwt::verify(
        &exchange_token,
        "https://gw.example",
        SECRET_A.as_bytes()
    )
    .is_none());
    assert_eq!(
        verify(h.auth(), &gateway_token, NOW + 1),
        None,
        "exchange verify must reject a gateway session token"
    );
}
