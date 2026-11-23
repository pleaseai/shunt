#![allow(clippy::await_holding_lock)] // Intentional cross-module test serialization.
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
    let env = format!("SHUNT_TEST_EXCHANGE_REORDER_{}", std::process::id());
    let toml_a = r#"
        tokens_env = "__ENV__"
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
        tokens_env = "__ENV__"
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
        let mut parsed: InboundAuthConfig =
            toml::from_str(&text.replace("__ENV__", &env)).expect("toml parses");
        parsed.exchange = table();
        parsed
    };
    let reorder = |text: &str| {
        let mut config = Config::default();
        config.server.auth = Some(parse(text));
        config
    };
    let _lock = crate::config::test_env_lock();
    std::env::remove_var(&env);
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

// --- `POST /v1/oauth/token`: failure rate limit ----------------------------

mod rate_limit {
    use std::net::SocketAddr;

    use axum::{
        body::Body,
        extract::{ConnectInfo, State},
        http::{header, HeaderMap, HeaderValue, StatusCode},
        Extension,
    };
    use serde_json::{json, Value};
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;
    use crate::auth::exchange::handler;
    use crate::auth::inbound_jwt::tests::{claims, issuer_serving, jwks, token};

    const GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

    /// A running gateway state whose only opted-in rule trusts a loopback issuer.
    struct Live {
        shared: crate::reload::SharedState,
        state: crate::server::AppState,
        issuer: MockServer,
        env: String,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    fn config_for(issuer: &str, env: &str, max_failures: u32, exchange: bool) -> Config {
        let mut rule = entry(issuer, exchange);
        rule.audience = StringList::One("shunt-clients".to_string());
        rule.require = [(
            "repository_owner".to_string(),
            StringList::One("pleaseai".into()),
        )]
        .into();
        rule.jwks_url = Some(format!("{issuer}/jwks"));
        let mut config = config_with(env, vec![rule], &[SECRET_A]);
        if !exchange {
            config.server.auth.as_mut().unwrap().exchange = None;
        }
        config.server.rate_limits.exchange_failure.max = max_failures;
        config
    }

    async fn live(tag: &str, max_failures: u32, issuer: MockServer) -> Live {
        let lock = crate::config::test_env_lock();
        let env = format!("SHUNT_TEST_EXCHANGE_RL_{tag}_{}", std::process::id());
        std::env::remove_var(&env);
        let config = config_for(&issuer.uri(), &env, max_failures, true);
        let stores = crate::gateway::GatewayStores::new(&config.server.rate_limits, None);
        let shared: crate::reload::SharedState = Arc::new(arc_swap::ArcSwap::from_pointee(
            crate::reload::RuntimeState::from_config(config).unwrap(),
        ));
        let state = crate::server::AppState::from_shared(
            shared.clone(),
            reqwest::Client::new(),
            Arc::new(crate::accounts::AccountPool::new()),
            Arc::new(crate::upstream_status::StatusStore::new()),
            Arc::new(crate::admin::AdminStores::new()),
            Arc::new(stores),
            Arc::new(crate::routing::stage::StageRouterStore::new()),
            Arc::new(JwksCache::new()),
            true,
        );
        Live {
            shared,
            state,
            issuer,
            env,
            _lock: lock,
        }
    }

    async fn serving(tag: &str, max_failures: u32) -> Live {
        live(tag, max_failures, issuer_serving(jwks()).await).await
    }

    fn assertion(issuer: &str) -> String {
        let mut claims = claims(issuer);
        claims["repository"] = json!("pleaseai/shunt");
        claims["repository_owner"] = json!("pleaseai");
        token(&claims)
    }

    fn exchange_body(assertion: &str) -> Vec<u8> {
        json!({ "grant_type": GRANT, "assertion": assertion })
            .to_string()
            .into_bytes()
    }

    struct Reply {
        status: StatusCode,
        headers: HeaderMap,
        body: Value,
    }

    impl Reply {
        fn error(&self) -> &str {
            self.body["error"].as_str().unwrap_or_default()
        }
    }

    async fn call(
        state: &crate::server::AppState,
        peer: &str,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) -> Reply {
        let mut headers = HeaderMap::new();
        if let Some(content_type) = content_type {
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(content_type).unwrap(),
            );
        }
        let peer: SocketAddr = format!("{peer}:4000").parse().unwrap();
        let response = handler::post(
            State(state.clone()),
            Some(Extension(ConnectInfo(peer))),
            headers,
            Body::from(body),
        )
        .await;
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        Reply {
            status: parts.status,
            headers: parts.headers,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    async fn post_json(state: &crate::server::AppState, peer: &str, body: Vec<u8>) -> Reply {
        call(state, peer, Some("application/json"), body).await
    }

    async fn jwks_fetches(issuer: &MockServer) -> usize {
        issuer.received_requests().await.unwrap_or_default().len()
    }

    fn assert_slow_down(reply: &Reply) {
        assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(reply.error(), "slow_down");
        assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
        let seconds: u64 = reply.headers[header::RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            (1..=600).contains(&seconds),
            "Retry-After {seconds} outside the window"
        );
    }

    #[tokio::test]
    async fn a_blocked_address_is_refused_before_any_verification() {
        let h = serving("BLOCK", 2).await;
        let valid = assertion(&h.issuer.uri());
        let bad_grant = json!({ "grant_type": "password", "assertion": valid }).to_string();

        for _ in 0..2 {
            let reply = post_json(&h.state, "10.0.0.1", bad_grant.clone().into_bytes()).await;
            assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        }
        assert_eq!(
            jwks_fetches(&h.issuer).await,
            0,
            "failures before the block never verified"
        );

        // The very next request carries a valid assertion and is still refused,
        // and no key set is fetched for it.
        let blocked = post_json(&h.state, "10.0.0.1", exchange_body(&valid)).await;
        assert_slow_down(&blocked);
        assert_eq!(
            jwks_fetches(&h.issuer).await,
            0,
            "a blocked request must not verify"
        );

        // The same assertion from another address succeeds, so the refusal above
        // was the limiter and not a bad assertion.
        let other = post_json(&h.state, "10.0.0.2", exchange_body(&valid)).await;
        assert_eq!(other.status, StatusCode::OK);
        assert_eq!(jwks_fetches(&h.issuer).await, 1);
    }

    #[tokio::test]
    async fn every_kind_of_failed_request_counts_against_the_address() {
        let h = serving("KINDS", 1).await;
        let valid = assertion(&h.issuer.uri());
        let mut wrong_aud = claims(&h.issuer.uri());
        wrong_aud["aud"] = json!("someone-else");
        let unverifiable = token(&wrong_aud);

        let kinds: Vec<(&str, Option<&str>, Vec<u8>, StatusCode)> = vec![
            (
                "wrong content type",
                Some("text/plain"),
                exchange_body(&valid),
                StatusCode::BAD_REQUEST,
            ),
            (
                "missing content type",
                None,
                exchange_body(&valid),
                StatusCode::BAD_REQUEST,
            ),
            (
                "malformed body",
                Some("application/json"),
                b"{not json".to_vec(),
                StatusCode::BAD_REQUEST,
            ),
            (
                "oversized body",
                Some("application/json"),
                vec![b'a'; 64 * 1024 + 1],
                StatusCode::BAD_REQUEST,
            ),
            (
                "unsupported grant type",
                Some("application/json"),
                json!({ "grant_type": "password", "assertion": valid })
                    .to_string()
                    .into_bytes(),
                StatusCode::BAD_REQUEST,
            ),
            (
                "unverifiable assertion",
                Some("application/json"),
                exchange_body(&unverifiable),
                StatusCode::UNAUTHORIZED,
            ),
        ];
        for (index, (name, content_type, body, expected)) in kinds.into_iter().enumerate() {
            let peer = format!("10.1.0.{}", index + 1);
            let first = call(&h.state, &peer, content_type, body).await;
            assert_eq!(
                first.status, expected,
                "{name}: the original response is kept"
            );
            let second = post_json(&h.state, &peer, exchange_body(&valid)).await;
            assert_slow_down(&second);
        }
    }

    #[tokio::test]
    async fn a_success_is_never_counted() {
        let h = serving("SUCCESS", 1).await;
        let valid = assertion(&h.issuer.uri());
        for _ in 0..4 {
            let reply = post_json(&h.state, "10.2.0.1", exchange_body(&valid)).await;
            assert_eq!(reply.status, StatusCode::OK);
        }
        // Had the successes counted, this first failure would already be a 429.
        let failed = post_json(&h.state, "10.2.0.1", b"{}".to_vec()).await;
        assert_eq!(failed.status, StatusCode::BAD_REQUEST);
        let blocked = post_json(&h.state, "10.2.0.1", exchange_body(&valid)).await;
        assert_slow_down(&blocked);
    }

    #[tokio::test]
    async fn an_unavailable_key_set_is_never_counted() {
        let issuer = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&issuer)
            .await;
        let h = live("DOWN", 1, issuer).await;
        let valid = assertion(&h.issuer.uri());
        for _ in 0..4 {
            let reply = post_json(&h.state, "10.3.0.1", exchange_body(&valid)).await;
            assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(reply.error(), "temporarily_unavailable");
        }
        // Had the 503s counted, this first real failure would already be a 429.
        let failed = post_json(&h.state, "10.3.0.1", b"{}".to_vec()).await;
        assert_eq!(failed.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_removed_table_answers_404_even_for_a_blocked_address() {
        let h = serving("REMOVED", 1).await;
        let valid = assertion(&h.issuer.uri());
        post_json(&h.state, "10.4.0.1", b"{}".to_vec()).await;
        assert_slow_down(&post_json(&h.state, "10.4.0.1", exchange_body(&valid)).await);

        let config = config_for(&h.issuer.uri(), &h.env, 1, false);
        h.shared.store(Arc::new(
            crate::reload::RuntimeState::from_config(config).unwrap(),
        ));
        let gone = post_json(&h.state, "10.4.0.1", exchange_body(&valid)).await;
        assert_eq!(gone.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_full_table_refuses_a_new_failing_address_with_slow_down() {
        let h = serving("FULL", 10).await;
        for index in 0..4096u32 {
            let peer = std::net::Ipv4Addr::from(0x0a10_0000 + index).to_string();
            let reply = call(&h.state, &peer, Some("text/plain"), Vec::new()).await;
            assert_eq!(
                reply.status,
                StatusCode::BAD_REQUEST,
                "address {index} fits"
            );
        }
        let overflow = call(&h.state, "10.99.0.1", Some("text/plain"), Vec::new()).await;
        assert_slow_down(&overflow);
        // An address already tracked still gets the ordinary answer.
        let tracked = call(&h.state, "10.16.0.0", Some("text/plain"), Vec::new()).await;
        assert_eq!(tracked.status, StatusCode::BAD_REQUEST);
    }
}
