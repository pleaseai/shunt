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

/// Whether `token` is *shaped* like a JWT this gateway issued: three
/// base64url segments whose payload carries `aud == "shunt"` or an `iss`
/// equal to this gateway's issuer — deliberately WITHOUT verifying the
/// signature or the expiry.
///
/// A do-not-forward decision asks "did we issue this?"; authentication asks
/// "is this valid right now?". They must not share an implementation: an
/// expired token, one minted by a sibling instance under a different
/// `public_url`, or one whose signature no longer matches after a secret
/// rotation, is still shunt's own credential and must never be relayed to a
/// third-party upstream. Forging `iss` to force a strip only removes the
/// forger's own credential, so the fail-safe direction is correct.
///
/// Never panics: any malformed input (wrong segment count, a header or
/// signature segment that is not base64url, non-base64 payload, non-UTF-8
/// bytes, non-JSON payload, JSON that is not an object) falls through to
/// `false`. The payload is parsed as a bare
/// [`serde_json::Value`] rather than deserialized into [`Claims`] — an
/// arbitrary third-party JWT is missing fields `Claims` requires and would
/// fail to deserialize, silently returning `false` for a token that does
/// carry a matching `aud` or `iss`.
pub fn has_shunt_shape(token: &str, issuer: &str) -> bool {
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature)) = (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    // `mint_at` encodes all three segments with `URL_SAFE_NO_PAD`, so requiring
    // the header and signature to decode cannot narrow this away from anything
    // shunt actually issued — it only keeps a value that merely *carries* a
    // shunt-shaped payload between two non-base64url segments from being
    // classified as a JWT at all.
    if URL_SAFE_NO_PAD.decode(header).is_err() || URL_SAFE_NO_PAD.decode(signature).is_err() {
        return false;
    }

    let Ok(decoded) = URL_SAFE_NO_PAD.decode(payload) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&decoded) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };

    object.get("aud").and_then(serde_json::Value::as_str) == Some(AUDIENCE)
        || object.get("iss").and_then(serde_json::Value::as_str) == Some(issuer)
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
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

    use super::{has_shunt_shape, mint_at, verify_at, HEADER_JSON};
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

    #[test]
    fn shape_matches_expired_token() {
        let token = mint_at(&identity(), "https://gateway.example", SECRET, 60, 1000);
        // Long expired, but still shunt-shaped: aud == "shunt".
        assert!(has_shunt_shape(&token, "https://gateway.example"));
        assert!(verify_at(&token, "https://gateway.example", SECRET, 999_999_999).is_none());
    }

    #[test]
    fn shape_matches_wrong_issuer_by_aud() {
        // Minted for a sibling instance under a different public_url, still
        // live. `verify_at` under this issuer rejects it, but its `aud` is
        // still "shunt" so it must still be recognized as shunt's own.
        let token = mint_at(&identity(), "https://sibling.example", SECRET, 3600, 1000);
        assert!(verify_at(&token, "https://gateway.example", SECRET, 1001).is_none());
        assert!(has_shunt_shape(&token, "https://gateway.example"));
    }

    #[test]
    fn shape_matches_by_issuer_when_aud_absent() {
        // A token whose payload lacks `aud` entirely (e.g. hand-forged, or
        // from a future claims shape) but whose `iss` matches this gateway.
        let header = URL_SAFE_NO_PAD.encode(HEADER_JSON);
        let payload = URL_SAFE_NO_PAD.encode(br#"{"iss":"https://gateway.example"}"#);
        let token = format!("{header}.{payload}.sig");
        assert!(has_shunt_shape(&token, "https://gateway.example"));
    }

    #[test]
    fn shape_matches_bad_signature() {
        // Well-formed, aud = "shunt", but signed under a different secret —
        // fails `verify_at`, must still be caught by shape.
        let token = mint_at(
            &identity(),
            "https://gateway.example",
            b"a-different-secret",
            3600,
            1000,
        );
        assert!(verify_at(&token, "https://gateway.example", SECRET, 1001).is_none());
        assert!(has_shunt_shape(&token, "https://gateway.example"));
    }

    #[test]
    fn shape_matches_regardless_of_token_length() {
        // The shape check is deliberately length-independent. A fast-fail
        // length cap here would fail *open*: `false` means "not shunt's own,
        // forward it upstream", so an oversized gateway JWT — claims are
        // built from IdP-supplied `sub`/`email`/`name` and a configured
        // `iss`, none of them bounded here — would be relayed to a
        // third-party upstream, which is the leak this check exists to
        // close. Bounding the work belongs at the HTTP header limit, not in
        // a predicate whose `false` branch forwards a credential.
        let mut identity = identity();
        identity.name = "n".repeat(8192);
        let token = mint_at(&identity, "https://gateway.example", SECRET, 3600, 1000);

        assert!(token.len() > 8192);
        assert!(has_shunt_shape(&token, "https://gateway.example"));
    }

    #[test]
    fn shape_rejects_ordinary_caller_credentials() {
        assert!(!has_shunt_shape(
            "sk-ant-api03-not-a-jwt-at-all",
            "https://gateway.example"
        ));
        assert!(!has_shunt_shape(
            "just-a-random-opaque-token",
            "https://gateway.example"
        ));
    }

    #[test]
    fn shape_rejects_malformed_input_without_panicking() {
        // Wrong segment count.
        assert!(!has_shunt_shape("a.b", "https://gateway.example"));
        assert!(!has_shunt_shape("a.b.c.d", "https://gateway.example"));
        // Garbage segments — non-base64 payload.
        assert!(!has_shunt_shape("a.b.c", "https://gateway.example"));
        // Valid base64, non-UTF-8 bytes.
        let header = URL_SAFE_NO_PAD.encode(HEADER_JSON);
        let non_utf8_payload = URL_SAFE_NO_PAD.encode([0xff, 0xfe, 0xfd]);
        let token = format!("{header}.{non_utf8_payload}.sig");
        assert!(!has_shunt_shape(&token, "https://gateway.example"));
        // Valid base64+UTF-8, but not JSON.
        let not_json_payload = URL_SAFE_NO_PAD.encode(b"not json");
        let token = format!("{header}.{not_json_payload}.sig");
        assert!(!has_shunt_shape(&token, "https://gateway.example"));
        // Valid JSON, but not an object.
        let array_payload = URL_SAFE_NO_PAD.encode(b"[1,2,3]");
        let token = format!("{header}.{array_payload}.sig");
        assert!(!has_shunt_shape(&token, "https://gateway.example"));
    }

    #[test]
    fn shape_requires_base64url_header_and_signature() {
        // A shunt-shaped payload wedged between segments that are not
        // base64url is not a JWT, so it is not something this gateway could
        // have minted — `mint_at` encodes every segment with URL_SAFE_NO_PAD.
        let payload = URL_SAFE_NO_PAD.encode(br#"{"aud":"shunt"}"#);
        let header = URL_SAFE_NO_PAD.encode(HEADER_JSON);

        // `+` and `=` are outside the URL-safe, unpadded alphabet.
        assert!(!has_shunt_shape(
            &format!("not+base64.{payload}.sig"),
            "https://gateway.example"
        ));
        assert!(!has_shunt_shape(
            &format!("{header}.{payload}.not+base64"),
            "https://gateway.example"
        ));
        // The same payload between well-formed segments still matches, so the
        // rejections above are the segment check and not the payload.
        assert!(has_shunt_shape(
            &format!("{header}.{payload}.sig"),
            "https://gateway.example"
        ));
    }

    #[test]
    fn shape_forged_issuer_only_removes_forgers_own_credential() {
        // A caller crafting aud/iss to force a strip only marks their own
        // value as shunt's — it can never leak someone else's, since the
        // check is per-value, not per-request.
        let header = URL_SAFE_NO_PAD.encode(HEADER_JSON);
        let payload = URL_SAFE_NO_PAD.encode(br#"{"aud":"shunt"}"#);
        let forged = format!("{header}.{payload}.sig");
        assert!(has_shunt_shape(&forged, "https://gateway.example"));
    }
}
