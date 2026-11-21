//! The gate's handling of shunt-issued workload exchange tokens: accepted from
//! `Authorization: Bearer` alone, as the identity the token carries, and never
//! ahead of a static token. Everything is network-free — the issuer's JWKS URL
//! points at a closed port, so a test that reached the JWT path would see
//! `Unavailable` rather than the verdict asserted.

use axum::http::{HeaderMap, HeaderValue};

use super::{authenticate, Outcome, Slots};
use crate::auth::exchange::{fingerprint, mint, unix_now};
use crate::auth::inbound::InboundAuth;
use crate::auth::inbound_jwt::JwksCache;
use crate::config::{ExchangeConfig, InboundAuthConfig, InboundJwtConfig, Secret, StringList};

const SECRET: &str = "gate-exchange-secret-0123456789abcdef";
const IDENTITY: &str = "pleaseai/shunt";
const JWT_HEADER: &str = "x-gateway-jwt";
const JWT_COOKIE: &str = "gw_session";
const ALL_SLOTS: [Slots; 3] = [Slots::Client, Slots::Codex, Slots::CodexDiscovery];

fn entry(exchange: bool) -> InboundJwtConfig {
    InboundJwtConfig {
        issuer: "https://token.actions.example".to_string(),
        audience: StringList::One("shunt-aud".to_string()),
        email_domains: Vec::new(),
        allowed_emails: Vec::new(),
        require: Default::default(),
        subject_prefix: StringList::One("repo:pleaseai/shunt:".to_string()),
        identity_claim: Some("repository".to_string()),
        algorithms: vec!["RS256".to_string()],
        authorized_parties: Vec::new(),
        clock_skew_seconds: 0,
        max_token_age_seconds: 3600,
        jwks_url: Some("http://127.0.0.1:9/jwks".to_string()),
        exchange,
    }
}

fn auth(tokens_env: &str, exchange: bool) -> InboundAuth {
    InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: tokens_env.to_string(),
        jwt: vec![entry(exchange)],
        jwt_header: Some(JWT_HEADER.to_string()),
        jwt_cookie: Some(JWT_COOKIE.to_string()),
        exchange: Some(ExchangeConfig {
            signing_secret: vec![Secret::from(SECRET)],
            max_ttl_seconds: 600,
        }),
    }
    .resolve()
    .expect("test inbound auth resolves")
}

fn issued(auth: &InboundAuth) -> String {
    mint(
        auth.exchange().unwrap(),
        IDENTITY,
        &fingerprint(&auth.jwt()[0]),
        unix_now(),
    )
    .token
}

fn headers(pairs: &[(&'static str, String)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.append(*name, HeaderValue::from_str(value).unwrap());
    }
    map
}

fn bearer(token: &str) -> HeaderMap {
    headers(&[("authorization", format!("Bearer {token}"))])
}

async fn gate(auth: &InboundAuth, headers: &HeaderMap, slots: Slots) -> Outcome {
    authenticate(auth, &JwksCache::new(), headers, slots).await
}

fn exchanged() -> Outcome {
    Outcome::Authenticated {
        client: IDENTITY.to_string(),
        static_token: false,
    }
}

#[tokio::test]
async fn an_issued_token_in_the_bearer_slot_authenticates_as_its_identity_on_every_posture() {
    let auth = auth("SHUNT_TEST_GATE_EXCHANGE_NONE_A", true);
    let headers = bearer(&issued(&auth));
    for slots in ALL_SLOTS {
        assert_eq!(gate(&auth, &headers, slots).await, exchanged(), "{slots:?}");
    }
}

#[tokio::test]
async fn an_issued_token_outside_the_bearer_slot_does_not_authenticate() {
    let auth = auth("SHUNT_TEST_GATE_EXCHANGE_NONE_B", true);
    let token = issued(&auth);
    let elsewhere = [
        headers(&[("x-api-key", token.clone())]),
        headers(&[("x-shunt-token", token.clone())]),
        headers(&[(JWT_HEADER, token.clone())]),
        headers(&[("cookie", format!("{JWT_COOKIE}={token}"))]),
        headers(&[("authorization", token.clone())]),
    ];
    for headers in &elsewhere {
        for slots in ALL_SLOTS {
            assert_eq!(
                gate(&auth, headers, slots).await,
                Outcome::Rejected,
                "{headers:?} {slots:?}"
            );
        }
    }
}

#[tokio::test]
async fn a_static_token_wins_over_an_exchange_token_in_the_same_request() {
    let env = "SHUNT_TEST_GATE_EXCHANGE_STATIC";
    let auth = {
        let _lock = crate::config::test_env_lock();
        std::env::set_var(env, "ci-static:s3cret");
        let resolved = auth(env, true);
        std::env::remove_var(env);
        resolved
    };
    let headers = headers(&[
        ("x-shunt-token", "s3cret".to_string()),
        ("authorization", format!("Bearer {}", issued(&auth))),
    ]);
    for slots in ALL_SLOTS {
        assert_eq!(
            gate(&auth, &headers, slots).await,
            Outcome::Authenticated {
                client: "ci-static".to_string(),
                static_token: true,
            },
            "{slots:?}"
        );
    }
}

#[tokio::test]
async fn an_expired_or_foreign_exchange_token_is_rejected_not_unavailable() {
    let auth = auth("SHUNT_TEST_GATE_EXCHANGE_NONE_C", true);
    let settings = auth.exchange().unwrap();
    let fp = fingerprint(&auth.jwt()[0]);
    let expired = mint(settings, IDENTITY, &fp, 1_000).token;
    let orphan = mint(settings, IDENTITY, "no-such-entry", unix_now()).token;
    let other_secret = {
        let other = crate::auth::exchange::ExchangeSettings::new(
            vec![b"another-secret-0123456789abcdef-xx".to_vec()],
            600,
        );
        mint(&other, IDENTITY, &fp, unix_now()).token
    };
    for token in [expired, orphan, other_secret] {
        assert_eq!(
            gate(&auth, &bearer(&token), Slots::Client).await,
            Outcome::Rejected
        );
    }
}

#[tokio::test]
async fn a_token_whose_entry_lost_its_opt_in_is_rejected() {
    let issuing = auth("SHUNT_TEST_GATE_EXCHANGE_NONE_D", true);
    let token = issued(&issuing);
    // Same secret, same entry, `exchange = false`: the snapshot after a reload
    // that cleared the opt-in. The exchange table stays, so only FR-8 rejects.
    let reloaded = InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: "SHUNT_TEST_GATE_EXCHANGE_NONE_D".to_string(),
        jwt: vec![entry(false), {
            let mut other = entry(true);
            other.issuer = "https://other.example".to_string();
            other
        }],
        exchange: Some(ExchangeConfig {
            signing_secret: vec![Secret::from(SECRET)],
            max_ttl_seconds: 600,
        }),
        ..Default::default()
    }
    .resolve()
    .unwrap();
    assert_eq!(
        gate(&reloaded, &bearer(&token), Slots::Client).await,
        Outcome::Rejected
    );
}

#[tokio::test]
async fn a_deployment_without_the_exchange_table_ignores_exchange_shaped_bearers() {
    let issuing = auth("SHUNT_TEST_GATE_EXCHANGE_NONE_E", true);
    let token = issued(&issuing);
    let plain = InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: "SHUNT_TEST_GATE_EXCHANGE_NONE_E".to_string(),
        jwt: vec![entry(false)],
        ..Default::default()
    }
    .resolve()
    .unwrap();
    assert_eq!(
        gate(&plain, &bearer(&token), Slots::Client).await,
        Outcome::Rejected
    );
}
