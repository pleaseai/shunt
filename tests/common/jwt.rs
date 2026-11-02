//! A signing key and a loopback JWKS issuer for the JWT sign-in integration
//! tests (`[server.auth] jwt_header` / `jwt_cookie`).
//!
//! This duplicates the fixed key and the claims/signing helpers of the unit
//! suite in `src/auth/inbound_jwt/tests.rs` on purpose: sharing them would take
//! a crate feature or a `#[path]` include, and ~30 copied lines cost less.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD, Engine};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

pub const AUDIENCE: &str = "shunt-clients";
const KID: &str = "key-one";

/// Public modulus of [`KEY_DER`], base64url: the `n` of the published JWK.
pub const MODULUS: &str = "qawicxxdtoL-YNU__uL1pWe-fVHvZgkjJAWs2QQkjxTfauwrSMHhKDF2O4C13H8Om5TOVxtuRNsziP8_lhWTPjCsNU2atmDJhLE5mjkIjVFMrCY58LmlpuG6VKdlBN2Y2V_8pG_bDNlj8eqHc8bWzQmiU8T4_Mr7sR_ZOtqeWwNl12Lh_stT8QuJ3AruKXxfvpWlupLQVhHDYA04hNsGl_pu7f5Hfbf0hwPv0hGOuTmOiUPB_3FzPahDth1oY6QAYC-To3qtL6WcQBERBMb8Iyc0BRiKvCYhcnJxt_J6XjqNxw7XSPvhtBX7HeZ69gqbiUNgpgwdXv5lzhu1f91ltQ";

/// PKCS#1 DER private key for the fixture key, base64. Fixed so the suite needs no
/// key generation; it signs nothing outside these tests.
const KEY_DER: &str = concat!(
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
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Claims for an Entra-shaped token: no `email`, the caller named by
/// `preferred_username`, and a `tid` the authorization rule requires.
pub fn claims(issuer: &str, identity: &str, lifetime_seconds: u64) -> serde_json::Value {
    let issued = now();
    serde_json::json!({
        "iss": issuer,
        "aud": AUDIENCE,
        "iat": issued,
        "exp": issued + lifetime_seconds,
        "sub": "subject-1",
        "preferred_username": identity,
        "tid": "t1",
    })
}

/// An RS256 token signed with the fixture key.
pub fn token(claims: &serde_json::Value) -> String {
    let der = STANDARD.decode(KEY_DER).expect("test key is valid base64");
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(KID.to_string());
    encode(&header, claims, &EncodingKey::from_rsa_der(&der)).expect("test token signs")
}

/// A loopback issuer serving the fixture JWKS at `/jwks`.
pub async fn issuer() -> MockServer {
    let server = MockServer::start().await;
    let document = serde_json::json!({"keys": [{
        "kty": "RSA", "use": "sig", "alg": "RS256", "kid": KID, "n": MODULUS, "e": "AQAB",
    }]});
    Mock::given(method("GET"))
        .and(path("/jwks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(document))
        .mount(&server)
        .await;
    server
}
