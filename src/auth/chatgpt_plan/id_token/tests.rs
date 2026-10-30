//! ID-token verification against a loopback issuer serving discovery + JWKS.

use base64::{engine::general_purpose::STANDARD, Engine};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::json;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use super::{verify_id_token, IdTokenError, VerifiedIdentity};
use crate::auth::inbound_jwt::JwksCache;

const CLIENT_ID: &str = "app_test_client";
const NONCE: &str = "attempt-nonce";

const KID_ONE: &str = "key-one";

const MODULUS_ONE: &str = "qawicxxdtoL-YNU__uL1pWe-fVHvZgkjJAWs2QQkjxTfauwrSMHhKDF2O4C13H8Om5TOVxtuRNsziP8_lhWTPjCsNU2atmDJhLE5mjkIjVFMrCY58LmlpuG6VKdlBN2Y2V_8pG_bDNlj8eqHc8bWzQmiU8T4_Mr7sR_ZOtqeWwNl12Lh_stT8QuJ3AruKXxfvpWlupLQVhHDYA04hNsGl_pu7f5Hfbf0hwPv0hGOuTmOiUPB_3FzPahDth1oY6QAYC-To3qtL6WcQBERBMb8Iyc0BRiKvCYhcnJxt_J6XjqNxw7XSPvhtBX7HeZ69gqbiUNgpgwdXv5lzhu1f91ltQ";

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

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
}

fn sign(claims: &serde_json::Value, kid: &str) -> String {
    let der = STANDARD
        .decode(KEY_ONE_DER)
        .expect("test key is valid base64");
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.to_string());
    encode(&header, claims, &EncodingKey::from_rsa_der(&der)).expect("test token signs")
}

/// A token shaped like OpenAI's: `aud` is an array, there is no `azp`, and
/// `exp - iat` is 3600.
fn claims(issuer: &str) -> serde_json::Value {
    let iat = now();
    json!({
        "iss": issuer,
        "aud": [CLIENT_ID],
        "nonce": NONCE,
        "sub": "auth0|user-1",
        "email": "dev@example.com",
        "email_verified": true,
        "iat": iat,
        "exp": iat + 3600,
    })
}

async fn issuer() -> MockServer {
    let server = MockServer::start().await;
    let uri = server.uri();
    Mock::given(method("GET"))
        .and(path("/.well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": uri,
            "jwks_uri": format!("{uri}/keys"),
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/keys"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "keys": [{
                "kty": "RSA", "use": "sig", "alg": "RS256",
                "kid": KID_ONE, "n": MODULUS_ONE, "e": "AQAB",
            }]
        })))
        .mount(&server)
        .await;
    server
}

async fn check(
    server: &MockServer,
    claims: &serde_json::Value,
    kid: &str,
) -> Result<VerifiedIdentity, IdTokenError> {
    let token = sign(claims, kid);
    verify_id_token(&JwksCache::new(), &server.uri(), CLIENT_ID, NONCE, &token).await
}

#[tokio::test]
async fn a_valid_token_is_accepted_and_yields_sub_and_email() {
    let server = issuer().await;
    let identity = check(&server, &claims(&server.uri()), KID_ONE).await;
    assert_eq!(
        identity,
        Ok(VerifiedIdentity {
            sub: "auth0|user-1".to_string(),
            email: Some("dev@example.com".to_string()),
        })
    );
}

#[tokio::test]
async fn a_token_without_email_yields_none() {
    let server = issuer().await;
    let mut body = claims(&server.uri());
    body.as_object_mut().unwrap().remove("email");
    let identity = check(&server, &body, KID_ONE).await.expect("accepted");
    assert_eq!(identity.email, None);
}

#[tokio::test]
async fn an_iat_five_seconds_in_the_future_is_accepted() {
    let server = issuer().await;
    let mut body = claims(&server.uri());
    let iat = now() + 5;
    body["iat"] = json!(iat);
    body["exp"] = json!(iat + 3600);
    assert!(check(&server, &body, KID_ONE).await.is_ok());
}

#[tokio::test]
async fn a_wrong_nonce_is_rejected() {
    let server = issuer().await;
    let mut body = claims(&server.uri());
    body["nonce"] = json!("another-attempt");
    assert_eq!(
        check(&server, &body, KID_ONE).await,
        Err(IdTokenError::Rejected)
    );
}

#[tokio::test]
async fn a_missing_nonce_is_rejected() {
    let server = issuer().await;
    let mut body = claims(&server.uri());
    body.as_object_mut().unwrap().remove("nonce");
    assert_eq!(
        check(&server, &body, KID_ONE).await,
        Err(IdTokenError::Rejected)
    );
}

#[tokio::test]
async fn a_wrong_audience_is_rejected() {
    let server = issuer().await;
    let mut body = claims(&server.uri());
    body["aud"] = json!(["someone-else"]);
    assert_eq!(
        check(&server, &body, KID_ONE).await,
        Err(IdTokenError::Rejected)
    );
}

#[tokio::test]
async fn an_exp_more_than_sixty_seconds_in_the_past_is_rejected() {
    let server = issuer().await;
    let mut body = claims(&server.uri());
    let exp = now() - 120;
    body["iat"] = json!(exp - 3600);
    body["exp"] = json!(exp);
    assert_eq!(
        check(&server, &body, KID_ONE).await,
        Err(IdTokenError::Rejected)
    );
}

#[tokio::test]
async fn an_exp_within_the_sixty_second_skew_is_accepted() {
    let server = issuer().await;
    let mut body = claims(&server.uri());
    let exp = now() - 20;
    body["iat"] = json!(exp - 3600);
    body["exp"] = json!(exp);
    assert!(check(&server, &body, KID_ONE).await.is_ok());
}

#[tokio::test]
async fn an_unknown_kid_is_rejected() {
    let server = issuer().await;
    assert_eq!(
        check(&server, &claims(&server.uri()), "rotated-away").await,
        Err(IdTokenError::Rejected)
    );
}

#[tokio::test]
async fn an_unreachable_key_set_is_reported_as_unavailable() {
    let server = MockServer::start().await;
    let body = claims(&server.uri());
    assert_eq!(
        check(&server, &body, KID_ONE).await,
        Err(IdTokenError::KeysUnavailable)
    );
}

#[tokio::test]
async fn errors_never_carry_the_token() {
    let server = issuer().await;
    let token = sign(&claims(&server.uri()), KID_ONE);
    for error in [IdTokenError::Rejected, IdTokenError::KeysUnavailable] {
        assert!(!format!("{error} {error:?}").contains(&token));
    }
}
