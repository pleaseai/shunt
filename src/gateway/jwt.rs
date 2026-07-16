use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::auth::inbound::constant_time_eq;

use super::approval::Identity;

const AUDIENCE: &str = "shunt";
const HEADER_JSON: &[u8] = br#"{"alg":"HS256","typ":"JWT"}"#;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub email: String,
    pub name: String,
    pub aud: String,
    pub iss: String,
    pub iat: u64,
    pub exp: u64,
}

pub fn mint(identity: &Identity, issuer: &str, secret: &[u8], ttl_seconds: u64) -> String {
    mint_at(identity, issuer, secret, ttl_seconds, unix_now())
}

pub fn verify(token: &str, issuer: &str, secret: &[u8]) -> Option<Claims> {
    verify_at(token, issuer, secret, unix_now())
}

fn mint_at(identity: &Identity, issuer: &str, secret: &[u8], ttl_seconds: u64, now: u64) -> String {
    let claims = Claims {
        sub: identity.sub.clone(),
        email: identity.email.clone(),
        name: identity.name.clone(),
        aud: AUDIENCE.to_string(),
        iss: issuer.to_string(),
        iat: now,
        exp: now.saturating_add(ttl_seconds),
    };
    let header = URL_SAFE_NO_PAD.encode(HEADER_JSON);
    let payload =
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("JWT claims serialize"));
    let signing_input = format!("{header}.{payload}");
    let signature = sign(signing_input.as_bytes(), secret);
    format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature))
}

fn verify_at(token: &str, issuer: &str, secret: &[u8], now: u64) -> Option<Claims> {
    let mut parts = token.split('.');
    let header = parts.next()?;
    let payload = parts.next()?;
    let signature = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let decoded_header = URL_SAFE_NO_PAD.decode(header).ok()?;
    if decoded_header != HEADER_JSON {
        return None;
    }
    let presented_signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
    let signing_input = format!("{header}.{payload}");
    let expected_signature = sign(signing_input.as_bytes(), secret);
    if !constant_time_eq(&presented_signature, &expected_signature) {
        return None;
    }

    let claims: Claims = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    if claims.aud != AUDIENCE || claims.iss != issuer || claims.exp <= now {
        return None;
    }
    Some(claims)
}

fn sign(message: &[u8], secret: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::{mint_at, verify_at};
    use crate::gateway::approval::Identity;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn identity() -> Identity {
        Identity {
            sub: "dev@example.com".into(),
            email: "dev@example.com".into(),
            name: "dev".into(),
        }
    }

    #[test]
    fn round_trips_claims() {
        let token = mint_at(&identity(), "https://gateway.example", SECRET, 3600, 1000);
        let claims =
            verify_at(&token, "https://gateway.example", SECRET, 1001).expect("valid token");

        assert_eq!(claims.sub, "dev@example.com");
        assert_eq!(claims.email, "dev@example.com");
        assert_eq!(claims.name, "dev");
        assert_eq!(claims.aud, "shunt");
        assert_eq!(claims.iss, "https://gateway.example");
        assert_eq!(claims.iat, 1000);
        assert_eq!(claims.exp, 4600);
    }

    #[test]
    fn rejects_tampering_and_wrong_issuer() {
        let token = mint_at(&identity(), "https://gateway.example", SECRET, 3600, 1000);
        let mut tampered = token.into_bytes();
        let index = tampered.len() - 1;
        tampered[index] = if tampered[index] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered).unwrap();

        assert!(verify_at(&tampered, "https://gateway.example", SECRET, 1001).is_none());
        let token = mint_at(&identity(), "https://gateway.example", SECRET, 3600, 1000);
        assert!(verify_at(&token, "https://other.example", SECRET, 1001).is_none());
    }

    #[test]
    fn rejects_expired_token_without_clock_skew() {
        let token = mint_at(&identity(), "https://gateway.example", SECRET, 60, 1000);
        assert!(verify_at(&token, "https://gateway.example", SECRET, 1059).is_some());
        assert!(verify_at(&token, "https://gateway.example", SECRET, 1060).is_none());
    }
}
