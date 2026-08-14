//! Verification behaviour for `[[server.auth.jwt]]`.
//!
//! The suite signs real RS256 tokens with fixed test keys and serves a real
//! JWKS document from a loopback mock, so every check exercises the same code
//! path a deployment does — including the network fetch, its cache, and the
//! refetch floor.

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use base64::{engine::general_purpose::STANDARD, Engine};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::json;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use crate::auth::gate::{self, Outcome, Slots};
use crate::auth::inbound::InboundAuth;
use crate::config::{InboundAuthConfig, InboundJwtConfig, StringList};

use super::{JwksCache, JwtOutcome};

const KID_ONE: &str = "key-one";
const KID_TWO: &str = "key-two";

/// Public modulus of [`KEY_ONE_DER`], base64url — the `n` of the JWK the mock
/// issuer publishes.
const MODULUS_ONE: &str = "qawicxxdtoL-YNU__uL1pWe-fVHvZgkjJAWs2QQkjxTfauwrSMHhKDF2O4C13H8Om5TOVxtuRNsziP8_lhWTPjCsNU2atmDJhLE5mjkIjVFMrCY58LmlpuG6VKdlBN2Y2V_8pG_bDNlj8eqHc8bWzQmiU8T4_Mr7sR_ZOtqeWwNl12Lh_stT8QuJ3AruKXxfvpWlupLQVhHDYA04hNsGl_pu7f5Hfbf0hwPv0hGOuTmOiUPB_3FzPahDth1oY6QAYC-To3qtL6WcQBERBMb8Iyc0BRiKvCYhcnJxt_J6XjqNxw7XSPvhtBX7HeZ69gqbiUNgpgwdXv5lzhu1f91ltQ";

/// PKCS#1 DER private key for `KID_ONE`, base64. Fixed so the suite needs no
/// key generation; it signs nothing outside these tests.
const KEY_ONE_DER: &str = concat!(
    "MIIEogIBAAKCAQEAqawicxxdtoL+YNU//uL1pWe+fVHvZgkjJAWs2QQkjxTfauwrSMHhKDF2",
    "O4C13H8Om5TOVxtuRNsziP8/lhWTPjCsNU2atmDJhLE5mjkIjVFMrCY58LmlpuG6VKdlBN2Y",
    "2V/8pG/bDNlj8eqHc8bWzQmiU8T4/Mr7sR/ZOtqeWwNl12Lh/stT8QuJ3AruKXxfvpWlupLQ",
    "VhHDYA04hNsGl/pu7f5Hfbf0hwPv0hGOuTmOiUPB/3FzPahDth1oY6QAYC+To3qtL6WcQBER",
    "BMb8Iyc0BRiKvCYhcnJxt/J6XjqNxw7XSPvhtBX7HeZ69gqbiUNgpgwdXv5lzhu1f91ltQID",
    "AQABAoIBAA8yXYQy3QqM8VdfR5Ppjw00CAyLFfvtGJUliaWG/faaF31TpCLS8/rYx0QCvgcp",
    "kszeMfGnrCOFqzgmKHNQVmJNE2IozmZvBSLvM/9mA/09nrY9LEECgl2P59Ok3zg62BxWwQxB",
    "/9FxonEsoNopS9w3Hy9ikSx5ymZDDylQIxEiokG9PSPxYuVZzl22+UQU6GKJaVvyUpGfxDGz",
    "RxTjIvTAMh7ziHvM2ItS84oHQguIZmKGBG3UYYLfYNZzLWtQ+3aghItOV9MhRPnHN5h8bJgT",
    "KgTjeRUf0G8puzqyQtdP5KLLtryR6qdHD98BzO+cr8kebZbGXPmnRpXyuS+BpOcCgYEA1qUy",
    "qsxKGPCkVYk68fWY1GJODbiKVve6ghVugCaO71kpL3jVkst0n6NcgsNgXV16wAjteNZ4VsOv",
    "Ror+RsDPto/lHOTORp65/dl9Ub/Juhep/Txz6SPZ+p3VEzBKKLgE1pjg94HFACYNTkJqAsnB",
    "OK1v5gjAjs3CVy5tvRm5WJcCgYEAylzE6VkFYmU6KEzLKNaHLIMj6m9iY3ZxsT1ifj1jnLoy",
    "3cIuB4n1KZMHVEz8xUfHw/Ft6zIU+ORR7ZlQ5jp30wARzaarZFBeiowpuPzWmVffe+/WOkAF",
    "uMC0nMEH1FMaRzBfqCFgkLAsX4OoM3b9dfDF0a+UCavKh6AB+jEOkZMCgYBRYAPbeOPGnMTQ",
    "oNw2CxRLwJEy5nmcCwMseg+Qig26dCUHGFpv8q5eL0LNWGDaRKxazYeqPjUVP87dgahxDnwx",
    "DFCiKaSCZX7B3IiES5+g64PIu/h9tNfZCalUQwR6d3luGjt/2jTjn4l/1/H06KRWZnp7zWmj",
    "OiKphrKX9H6uNQKBgDlhOriL9HnlCCubMtQemG+ns8xqzvQzBqPiKwZus8siBQBaaiDbHngu",
    "Z5qgxd/OrbdCww84wTedzhlYKtdNZuKel22/v8OPAm+4tK/uiY8rmoQTCqSzuKudgNkd5vFu",
    "qvnanpUW+cGtIrfmphAJwm7p2b3OUmS3oJL6bPUbae0fAoGAIj+J0UUJxmw9XgXJ9/L4U8gv",
    "irYZqyDc6jF0rqCUh/gdyxMaRL+lHtJeMz/SvhXIEKXXaRIPQv7Kb8K0GauatfXXfK6qLRdg",
    "K3FCkHxuBouK6CjsbCxA+AibfXYOzmVDb662VaX/uLmhfm5/6Rsfe2sOQ16DOCFGPk5L1YFH",
    "A7s=",
);

/// A second, unrelated key: proves a token signed by the wrong key is
/// rejected even when its `kid` names a key the issuer publishes.
const KEY_TWO_DER: &str = concat!(
    "MIIEowIBAAKCAQEAtXbZUPAcX/LeRux8dhpDaxkbDU0IRXR4lvGe9egSE+H6XbvBGRPgvvmw",
    "lExQTZlu18wlxvRz9ix1FNOgtM4jj7Jt3GrHmz/tTwPZ4Ui6QbGbXdtsqtRkIy/AwLJyCcwr",
    "Gg3Jq0T8D2OsvQ5mCjm6ynMZsVsofgINB2ArxGqZrwz9r4Vwk5NC24qZ7g+BGsqPjS1NOG19",
    "fx/hP/+1aOKTq8KpEFLkMmXSEOxmqAJIqogufzu0mQnbm222GAUVyfGRtjY4gBrOgsP/G1tY",
    "84zmDsmR3Iq5PULxioqbRw+MRyruik8wRS80zjWNoCTzDYIqS01W1x8SZmBrW3zSG1TMZwID",
    "AQABAoIBABFSWM3XkGB+TpvG4N3l2uVNvl3SKnB1fOStCByhZvGxQqoQ4o6nbZl9PHmjdo+P",
    "DlwYt3XFiY4y/Igc6BG/kvhusqWgJxFEPxL++JzUdDxncj+5fXwpuFdS1xuYd7dEtHaRC1zx",
    "wafMaXBF1WqzdlzzM2gmc0037+Yc1tYHIPLAabRlx5eeDDc71iJwPnqX6cHPpPKo4AbLMlkc",
    "6ATcj0IMiSNPAlviyxi720e8BG/zaUgWRTa3JZKKBU3s9rbc1uHoHj6txAbaHyxv9QlY2zCD",
    "MPbEIcIvAerjU+pG3d8PoAD9vCPgy5Dx7nzX0LBP9qwcVZW/dVYliZb7rb5LuoECgYEA3mk1",
    "qRNHyAJR4KxKfioTtoZOxt9hBwT+h+KElnjglJtNGwnXd9e/dAoYBgUyZWxmKebYDj0rGoWP",
    "MgZzdIL+0QGVuL8swoMTfiI97es/gVz5UDAatX4dBFZaQ9dGe5oBbvSoea4LKtqdR0RZkWh0",
    "XFoNx0HKISA3F5t93gmVfYECgYEA0N6RlPzF87hTO5Tkv+4/7aRqWBEFGJh1NKjGAll5RY3y",
    "kfCRvT9GFfgWBbX4xLIlRL9BivgYR00k2K74bviL9DdPiu1BK83JO7CoUkhiKN9Nj2EHRHcZ",
    "6zB38vnmC5yku1NeW62PzwjmyvtSkndPjYPIPQ+m8kI7NQYQQsiRDecCgYEAydCWDJGeNPNF",
    "8KTmA42SzbEZkoPnu0Lg49S7kv6karRxRvOrPOfcpiLyoaPdkwLFwYfizSjcD/jZcv8/jJ3B",
    "M05I2Zc/ulDOQ0o2/8jTm0MOR6Ee20lQczsYNS8GmempG1GN/rvbDkvJI331+GfcDmD417Hv",
    "BBgDZbyGfhAcQgECgYAK+edEoRP1/tXA584tl+OcJWvBPQO7iyd9oPDm5rTMxuzcZnwCKfUQ",
    "6nydyDZOf94dgr97AhakiJVLHcbIbh9Msitn7ZfjKWlNzbbKvCsMYs+8nSi4nFmsVlu3VRKV",
    "waGWhocK4lAQXTNjr1ljgQmZMjevncb0LC7YVn08RTX6OQKBgD9FGyTa1GHBJ22kh6+EU/ZM",
    "AEkyqI78jlW147i0L6VN7qZTbokY5cq0OVFE55kGTuqrV21F+ljn2jIV1flBIQ+CByMAOHFx",
    "DmvuQGmgYmTsjZ2brEwGbb0rOJxlugVtQJgKmKcdYZ5wpZdfk9c/ZdV74p9mEv+iWr5rV7X1",
    "fXMj",
);

/// Far enough in the future that the suite does not expire, and stable so a
/// failure never depends on when it ran.
const FUTURE_EXP: u64 = 4_000_000_000;

/// Claims builder. Every test starts from a token that *should* pass, then
/// breaks exactly one thing, so a passing rejection test cannot be passing for
/// an unrelated reason.
fn claims(issuer: &str) -> serde_json::Value {
    json!({
        "iss": issuer,
        "aud": "shunt-clients",
        "exp": FUTURE_EXP,
        "iat": FUTURE_EXP - 300,
        "sub": "user-1",
        "email": "dev@example.com",
        "email_verified": true,
    })
}

fn sign(claims: &serde_json::Value, der_b64: &str, kid: &str, algorithm: Algorithm) -> String {
    let der = STANDARD.decode(der_b64).expect("test key is valid base64");
    let mut header = Header::new(algorithm);
    header.kid = Some(kid.to_string());
    encode(&header, claims, &EncodingKey::from_rsa_der(&der)).expect("test token signs")
}

fn token(claims: &serde_json::Value) -> String {
    sign(claims, KEY_ONE_DER, KID_ONE, Algorithm::RS256)
}

fn jwks() -> serde_json::Value {
    json!({
        "keys": [{
            "kty": "RSA",
            "use": "sig",
            "alg": "RS256",
            "kid": KID_ONE,
            "n": MODULUS_ONE,
            "e": "AQAB",
        }]
    })
}

/// A loopback issuer serving a JWKS at `/jwks`.
async fn issuer_serving(document: serde_json::Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(document))
        .mount(&server)
        .await;
    server
}

fn entry(issuer: &str, jwks_url: &str) -> InboundJwtConfig {
    InboundJwtConfig {
        issuer: issuer.to_string(),
        audience: StringList::One("shunt-clients".to_string()),
        email_domains: vec!["example.com".to_string()],
        allowed_emails: Vec::new(),
        algorithms: vec!["RS256".to_string()],
        authorized_parties: Vec::new(),
        clock_skew_seconds: 0,
        max_token_age_seconds: 3600,
        jwks_url: Some(jwks_url.to_string()),
    }
}

/// Resolve through the real config path so every test also covers validation
/// and normalization, not just the runtime checks.
fn auth_with(entries: Vec<InboundJwtConfig>, tokens_env: Option<&str>) -> InboundAuth {
    let env = tokens_env.unwrap_or("SHUNT_TEST_JWT_NO_TOKENS");
    InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: env.to_string(),
        jwt: entries,
    }
    .resolve()
    .expect("test inbound auth resolves")
}

fn bearer(token: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    headers
}

async fn verify_with(auth: &InboundAuth, token: &str) -> JwtOutcome {
    super::verify(auth.jwt(), &JwksCache::new(), token).await
}

#[tokio::test]
async fn valid_token_authenticates_and_resolves_the_email_as_identity() {
    let server = issuer_serving(jwks()).await;
    let auth = auth_with(
        vec![entry(&server.uri(), &format!("{}/jwks", server.uri()))],
        None,
    );

    let outcome = verify_with(&auth, &token(&claims(&server.uri()))).await;

    assert_eq!(
        outcome,
        JwtOutcome::Verified {
            identity: "dev@example.com".to_string(),
            issuer: server.uri(),
        }
    );
}

#[tokio::test]
async fn wrong_audience_rejects() {
    let server = issuer_serving(jwks()).await;
    let auth = auth_with(
        vec![entry(&server.uri(), &format!("{}/jwks", server.uri()))],
        None,
    );
    let mut claims = claims(&server.uri());
    claims["aud"] = json!("some-other-client");

    assert_eq!(
        verify_with(&auth, &token(&claims)).await,
        JwtOutcome::Rejected
    );
}

#[tokio::test]
async fn unconfigured_issuer_rejects_without_reaching_any_key_set() {
    let server = issuer_serving(jwks()).await;
    let auth = auth_with(
        vec![entry(&server.uri(), &format!("{}/jwks", server.uri()))],
        None,
    );
    let mut claims = claims(&server.uri());
    claims["iss"] = json!("https://issuer.invalid");

    assert_eq!(
        verify_with(&auth, &token(&claims)).await,
        JwtOutcome::Rejected
    );
    // No entry matched, so the JWKS was never fetched.
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn expired_token_rejects() {
    let server = issuer_serving(jwks()).await;
    let auth = auth_with(
        vec![entry(&server.uri(), &format!("{}/jwks", server.uri()))],
        None,
    );
    let mut claims = claims(&server.uri());
    claims["iat"] = json!(1_000_000);
    claims["exp"] = json!(1_000_300);

    assert_eq!(
        verify_with(&auth, &token(&claims)).await,
        JwtOutcome::Rejected
    );
}

#[tokio::test]
async fn token_older_than_max_token_age_rejects() {
    let server = issuer_serving(jwks()).await;
    let mut config = entry(&server.uri(), &format!("{}/jwks", server.uri()));
    config.max_token_age_seconds = 600;
    let auth = auth_with(vec![config], None);

    // Unexpired, but minted with a 2-hour lifetime: shunt keeps no revocation
    // state, so a long-lived grant is refused outright.
    let mut long_lived = claims(&server.uri());
    long_lived["iat"] = json!(FUTURE_EXP - 7200);
    assert_eq!(
        verify_with(&auth, &token(&long_lived)).await,
        JwtOutcome::Rejected
    );

    // The same token inside the bound is accepted, so the rejection above is
    // attributable to the age check and nothing else.
    let mut short_lived = claims(&server.uri());
    short_lived["iat"] = json!(FUTURE_EXP - 300);
    assert!(matches!(
        verify_with(&auth, &token(&short_lived)).await,
        JwtOutcome::Verified { .. }
    ));
}

#[tokio::test]
async fn unknown_kid_rejects() {
    let server = issuer_serving(jwks()).await;
    let auth = auth_with(
        vec![entry(&server.uri(), &format!("{}/jwks", server.uri()))],
        None,
    );
    let forged = sign(
        &claims(&server.uri()),
        KEY_ONE_DER,
        "not-published",
        Algorithm::RS256,
    );

    assert_eq!(verify_with(&auth, &forged).await, JwtOutcome::Rejected);
}

#[tokio::test]
async fn token_signed_with_another_key_rejects() {
    let server = issuer_serving(jwks()).await;
    let auth = auth_with(
        vec![entry(&server.uri(), &format!("{}/jwks", server.uri()))],
        None,
    );
    // Correct `kid`, wrong signing key: only the signature check can catch this.
    let forged = sign(
        &claims(&server.uri()),
        KEY_TWO_DER,
        KID_ONE,
        Algorithm::RS256,
    );

    assert_eq!(verify_with(&auth, &forged).await, JwtOutcome::Rejected);
}

#[tokio::test]
async fn a_header_algorithm_outside_the_configured_pin_rejects() {
    // The pin itself, isolated: same issuer, same key, same `kid`, and an
    // algorithm the key can genuinely produce — only `validation.algorithms`
    // separates it from the accepted token. Assert the pin is what rejects it
    // rather than trusting the HMAC case below, where the key type would
    // refuse the token even with a broken pin.
    let server = issuer_serving(jwks()).await;
    let auth = auth_with(
        vec![entry(&server.uri(), &format!("{}/jwks", server.uri()))],
        None,
    );
    let claims = claims(&server.uri());

    let pinned = sign(&claims, KEY_ONE_DER, KID_ONE, Algorithm::RS256);
    assert!(matches!(
        verify_with(&auth, &pinned).await,
        JwtOutcome::Verified { .. }
    ));

    let unpinned = sign(&claims, KEY_ONE_DER, KID_ONE, Algorithm::RS384);
    assert_eq!(verify_with(&auth, &unpinned).await, JwtOutcome::Rejected);
}

#[tokio::test]
async fn hmac_token_signed_with_the_published_modulus_rejects() {
    // `alg` confusion end to end: the attacker takes the public key the issuer
    // publishes and uses it as an HMAC secret, hoping the header's `alg`
    // selects the algorithm. Two independent layers refuse this — the
    // configured pin (asserted above) and the fact that config validation
    // never lets a symmetric algorithm into `algorithms` at all (asserted in
    // `config.rs`). This test fixes the outcome of the actual attack.
    let server = issuer_serving(jwks()).await;
    let auth = auth_with(
        vec![entry(&server.uri(), &format!("{}/jwks", server.uri()))],
        None,
    );
    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some(KID_ONE.to_string());
    let forged = encode(
        &header,
        &claims(&server.uri()),
        &EncodingKey::from_secret(MODULUS_ONE.as_bytes()),
    )
    .expect("HMAC test token signs");

    assert_eq!(verify_with(&auth, &forged).await, JwtOutcome::Rejected);
}

#[tokio::test]
async fn email_domain_matches_the_domain_part_not_a_suffix() {
    let server = issuer_serving(jwks()).await;
    let auth = auth_with(
        vec![entry(&server.uri(), &format!("{}/jwks", server.uri()))],
        None,
    );

    let mut lookalike = claims(&server.uri());
    lookalike["email"] = json!("attacker@notexample.com");
    assert_eq!(
        verify_with(&auth, &token(&lookalike)).await,
        JwtOutcome::Rejected
    );

    // Case-insensitive on the real domain, so the rejection above is about the
    // domain part and not about matching being strict everywhere.
    let mut mixed_case = claims(&server.uri());
    mixed_case["email"] = json!("Dev@Example.COM");
    assert!(matches!(
        verify_with(&auth, &token(&mixed_case)).await,
        JwtOutcome::Verified { .. }
    ));
}

#[tokio::test]
async fn unverified_email_rejects() {
    let server = issuer_serving(jwks()).await;
    let auth = auth_with(
        vec![entry(&server.uri(), &format!("{}/jwks", server.uri()))],
        None,
    );
    let mut claims = claims(&server.uri());
    claims["email_verified"] = json!(false);

    assert_eq!(
        verify_with(&auth, &token(&claims)).await,
        JwtOutcome::Rejected
    );
}

#[tokio::test]
async fn azp_must_match_when_present() {
    let server = issuer_serving(jwks()).await;
    let auth = auth_with(
        vec![entry(&server.uri(), &format!("{}/jwks", server.uri()))],
        None,
    );

    let mut foreign = claims(&server.uri());
    foreign["azp"] = json!("another-client");
    assert_eq!(
        verify_with(&auth, &token(&foreign)).await,
        JwtOutcome::Rejected
    );

    // `authorized_parties` defaults to `audience`, so the token's own audience
    // is an accepted `azp`.
    let mut own = claims(&server.uri());
    own["azp"] = json!("shunt-clients");
    assert!(matches!(
        verify_with(&auth, &token(&own)).await,
        JwtOutcome::Verified { .. }
    ));
}

#[tokio::test]
async fn two_entries_may_share_one_issuer_with_different_audiences() {
    let server = issuer_serving(jwks()).await;
    let jwks_url = format!("{}/jwks", server.uri());
    let mut humans = entry(&server.uri(), &jwks_url);
    humans.audience = StringList::One("humans".to_string());
    let mut services = entry(&server.uri(), &jwks_url);
    services.audience = StringList::One("shunt-clients".to_string());
    // The matching entry is second: selection must collect every entry for the
    // issuer, not stop at the first.
    let auth = auth_with(vec![humans, services], None);

    assert!(matches!(
        verify_with(&auth, &token(&claims(&server.uri()))).await,
        JwtOutcome::Verified { .. }
    ));
}

#[tokio::test]
async fn unreachable_key_set_is_unavailable_and_refetches_at_most_once_per_window() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let auth = auth_with(
        vec![entry(&server.uri(), &format!("{}/jwks", server.uri()))],
        None,
    );
    let cache = JwksCache::new();
    let token = token(&claims(&server.uri()));

    // A `401` here would tell an operator their credential is wrong when their
    // IdP is down.
    assert_eq!(
        super::verify(auth.jwt(), &cache, &token).await,
        JwtOutcome::Unavailable
    );
    // Still unavailable rather than silently downgrading to a rejection, and
    // the failing issuer is not hammered.
    assert_eq!(
        super::verify(auth.jwt(), &cache, &token).await,
        JwtOutcome::Unavailable
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn one_unreachable_issuer_does_not_deny_another() {
    let good = issuer_serving(jwks()).await;
    let bad = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&bad)
        .await;
    let auth = auth_with(
        vec![
            entry(&bad.uri(), &format!("{}/jwks", bad.uri())),
            entry(&good.uri(), &format!("{}/jwks", good.uri())),
        ],
        None,
    );
    let cache = JwksCache::new();

    assert!(matches!(
        super::verify(auth.jwt(), &cache, &token(&claims(&good.uri()))).await,
        JwtOutcome::Verified { .. }
    ));
    assert_eq!(
        super::verify(auth.jwt(), &cache, &token(&claims(&bad.uri()))).await,
        JwtOutcome::Unavailable
    );
}

#[tokio::test]
async fn an_unknown_kid_refetches_once_then_serves_from_cache() {
    let server = issuer_serving(jwks()).await;
    let auth = auth_with(
        vec![entry(&server.uri(), &format!("{}/jwks", server.uri()))],
        None,
    );
    let cache = JwksCache::new();
    let forged = sign(
        &claims(&server.uri()),
        KEY_ONE_DER,
        KID_TWO,
        Algorithm::RS256,
    );

    for _ in 0..3 {
        assert_eq!(
            super::verify(auth.jwt(), &cache, &forged).await,
            JwtOutcome::Rejected
        );
    }
    // Without the floor, each forged `kid` would be a fetch against the issuer.
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn the_key_set_is_fetched_once_and_reused() {
    let server = issuer_serving(jwks()).await;
    let auth = auth_with(
        vec![entry(&server.uri(), &format!("{}/jwks", server.uri()))],
        None,
    );
    let cache = JwksCache::new();
    let token = token(&claims(&server.uri()));

    for _ in 0..3 {
        assert!(matches!(
            super::verify(auth.jwt(), &cache, &token).await,
            JwtOutcome::Verified { .. }
        ));
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn the_key_set_is_discovered_when_no_jwks_url_is_configured() {
    let server = MockServer::start().await;
    let issuer = server.uri();
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": issuer,
            "jwks_uri": format!("{issuer}/keys"),
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/keys"))
        .respond_with(ResponseTemplate::new(200).set_body_json(jwks()))
        .mount(&server)
        .await;
    let mut config = entry(&issuer, "");
    config.jwks_url = None;
    let auth = auth_with(vec![config], None);

    assert!(matches!(
        verify_with(&auth, &token(&claims(&issuer))).await,
        JwtOutcome::Verified { .. }
    ));
}

#[tokio::test]
async fn discovery_whose_issuer_disagrees_is_refused() {
    let server = MockServer::start().await;
    let issuer = server.uri();
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": "https://somewhere.else",
            "jwks_uri": format!("{issuer}/keys"),
        })))
        .mount(&server)
        .await;
    let mut config = entry(&issuer, "");
    config.jwks_url = None;
    let auth = auth_with(vec![config], None);

    // No key set could be resolved at all, so this is an outage, not a bad
    // credential.
    assert_eq!(
        verify_with(&auth, &token(&claims(&issuer))).await,
        JwtOutcome::Unavailable
    );
}

#[tokio::test]
async fn a_malformed_credential_is_rejected_without_any_fetch() {
    let server = issuer_serving(jwks()).await;
    let auth = auth_with(
        vec![entry(&server.uri(), &format!("{}/jwks", server.uri()))],
        None,
    );
    let cache = JwksCache::new();

    for candidate in ["", "not-a-jwt", "a.b.c", "Bearer"] {
        assert_eq!(
            super::verify(auth.jwt(), &cache, candidate).await,
            JwtOutcome::Rejected,
            "{candidate:?}"
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_token_without_kid_is_rejected() {
    let server = issuer_serving(jwks()).await;
    let auth = auth_with(
        vec![entry(&server.uri(), &format!("{}/jwks", server.uri()))],
        None,
    );
    let der = STANDARD.decode(KEY_ONE_DER).unwrap();
    // No `kid`: shunt refuses to try every published key.
    let anonymous = encode(
        &Header::new(Algorithm::RS256),
        &claims(&server.uri()),
        &EncodingKey::from_rsa_der(&der),
    )
    .unwrap();

    assert_eq!(verify_with(&auth, &anonymous).await, JwtOutcome::Rejected);
}

#[tokio::test]
async fn the_gate_accepts_a_static_token_and_a_jwt_on_the_same_route() {
    let env = "SHUNT_TEST_JWT_MIXED_TOKENS";
    std::env::set_var(env, "ci:static-token");
    let server = issuer_serving(jwks()).await;
    let auth = auth_with(
        vec![entry(&server.uri(), &format!("{}/jwks", server.uri()))],
        Some(env),
    );
    std::env::remove_var(env);
    let cache = JwksCache::new();

    let mut static_headers = HeaderMap::new();
    static_headers.insert(
        HeaderName::from_static("x-shunt-token"),
        HeaderValue::from_static("static-token"),
    );
    assert_eq!(
        gate::authenticate(&auth, &cache, &static_headers, Slots::Client).await,
        Outcome::Authenticated {
            client: "ci".to_string(),
            static_token: true,
        }
    );

    assert_eq!(
        gate::authenticate(
            &auth,
            &cache,
            &bearer(&token(&claims(&server.uri()))),
            Slots::Client,
        )
        .await,
        Outcome::Authenticated {
            client: "dev@example.com".to_string(),
            static_token: false,
        }
    );

    // Neither credential present, and a wrong one, both collapse to the same
    // rejection — nothing distinguishes which check failed.
    assert_eq!(
        gate::authenticate(&auth, &cache, &HeaderMap::new(), Slots::Client).await,
        Outcome::Rejected
    );
    assert_eq!(
        gate::authenticate(&auth, &cache, &bearer("nonsense"), Slots::Client).await,
        Outcome::Rejected
    );
}

#[tokio::test]
async fn the_gate_reports_an_unreachable_issuer_as_unavailable() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let auth = auth_with(
        vec![entry(&server.uri(), &format!("{}/jwks", server.uri()))],
        None,
    );

    assert_eq!(
        gate::authenticate(
            &auth,
            &JwksCache::new(),
            &bearer(&token(&claims(&server.uri()))),
            Slots::Client,
        )
        .await,
        Outcome::Unavailable
    );
}

#[test]
fn the_unverified_issuer_is_read_for_routing_only() {
    // Selection reads `iss` before any signature check; a payload that is not
    // JSON, or carries no `iss`, simply selects nothing.
    let claims = claims("https://issuer.example");
    let token = token(&claims);
    assert_eq!(
        super::unverified_issuer(&token).as_deref(),
        Some("https://issuer.example")
    );
    assert_eq!(super::unverified_issuer("a.!!!.c"), None);
    assert_eq!(super::unverified_issuer("only-one-segment"), None);
}

#[test]
fn the_identity_is_bounded_and_stays_valid_utf8() {
    assert_eq!(
        super::bounded_identity("dev@example.com"),
        "dev@example.com"
    );
    let long = format!("{}@example.com", "\u{00e9}".repeat(400));
    let bounded = super::bounded_identity(&long);
    assert!(bounded.len() <= super::MAX_IDENTITY_BYTES);
    // Truncation lands on a char boundary rather than splitting the multi-byte
    // character that straddles the cap.
    assert!(long.starts_with(&bounded));
}

#[test]
fn only_safe_key_set_endpoints_are_accepted() {
    assert!(super::validate_endpoint("https://issuer.example/jwks").is_ok());
    assert!(super::validate_endpoint("http://127.0.0.1:9000/jwks").is_ok());
    assert!(super::validate_endpoint("http://issuer.example/jwks").is_err());
    assert!(super::validate_endpoint("https://user:pw@issuer.example/jwks").is_err());
    assert!(super::validate_endpoint("https://issuer.example/jwks#frag").is_err());
    assert!(super::validate_endpoint("not-a-url").is_err());
}

#[tokio::test]
async fn a_gated_route_answers_401_for_a_bad_jwt_and_503_when_the_issuer_is_down() {
    // The gate's two rejection arms as HTTP, on a real handler: everything so
    // far asserted the `Outcome`, and the status codes are what an operator
    // actually sees.
    use axum::extract::State;

    let env = unset_tokens_env("ROUTER");
    let down = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&down)
        .await;

    let mut config = crate::config::Config::default();
    config.server.auth = Some(InboundAuthConfig {
        header: "x-shunt-token".to_string(),
        tokens_env: env,
        jwt: vec![entry(&down.uri(), &format!("{}/jwks", down.uri()))],
    });
    let state = crate::server::AppState::new(config, reqwest::Client::new()).unwrap();

    // No credential at all: nothing to verify, so no issuer is consulted.
    let response = crate::discovery::get(State(state.clone()), HeaderMap::new()).await;
    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);

    // A well-formed token for the configured issuer, whose key set is down.
    let response = crate::discovery::get(State(state), bearer(&token(&claims(&down.uri())))).await;
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
}

/// An env var name guaranteed unset, so `tokens_env` resolves empty and the
/// JWT entry is the only credential.
fn unset_tokens_env(tag: &str) -> String {
    let env = format!("SHUNT_TEST_JWT_NO_TOKENS_{tag}_{}", std::process::id());
    std::env::remove_var(&env);
    env
}

#[tokio::test]
async fn a_reload_swaps_the_issuer_set_but_keeps_the_key_cache() {
    // Two properties at once, because they pull against each other: entries
    // must be re-resolved on reload, and the JWKS cache must *not* be, or a
    // reload would refetch every configured issuer (and could be repeated to
    // make shunt do so).
    use std::sync::Arc;

    let first = issuer_serving(jwks()).await;
    let second = issuer_serving(jwks()).await;
    let env = unset_tokens_env("RELOAD");

    let config_with = |issuer: &MockServer| {
        let mut config = crate::config::Config::default();
        config.server.auth = Some(InboundAuthConfig {
            header: "x-shunt-token".to_string(),
            tokens_env: env.clone(),
            jwt: vec![entry(&issuer.uri(), &format!("{}/jwks", issuer.uri()))],
        });
        config
    };

    let shared: crate::reload::SharedState = Arc::new(arc_swap::ArcSwap::from_pointee(
        crate::reload::RuntimeState::from_config(config_with(&first)).unwrap(),
    ));
    let state = crate::server::AppState::from_shared(
        shared.clone(),
        reqwest::Client::new(),
        Arc::new(crate::accounts::AccountPool::new()),
        Arc::new(crate::upstream_status::StatusStore::new()),
        Arc::new(crate::admin::AdminStores::new()),
        Arc::new(crate::gateway::GatewayStores::default()),
        Arc::new(JwksCache::new()),
        true,
    );
    assert_eq!(
        state.inbound_auth.as_ref().unwrap().jwt()[0].issuer,
        first.uri()
    );

    shared.store(Arc::new(
        crate::reload::RuntimeState::from_config(config_with(&second)).unwrap(),
    ));
    let reloaded = state.refreshed();

    assert_eq!(
        reloaded.inbound_auth.as_ref().unwrap().jwt()[0].issuer,
        second.uri(),
        "the reloaded issuer must replace the previous one"
    );
    assert!(
        Arc::ptr_eq(&state.inbound_jwks, &reloaded.inbound_jwks),
        "the JWKS cache must survive the reload"
    );
}
