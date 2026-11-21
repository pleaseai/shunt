//! HS256 compact-JWT primitives shared by every token family shunt itself
//! mints (gateway sessions, workload exchange tokens).
//!
//! Only the signature envelope lives here: the fixed header, signing, and the
//! constant-time signature check. Each family owns its claims and decides
//! what makes a payload acceptable, so neither verifier can drift into
//! accepting the other's tokens through shared claim logic.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::auth::inbound::constant_time_eq;

/// The only header shunt mints and the only header it accepts.
pub(crate) const HEADER_JSON: &[u8] = br#"{"alg":"HS256","typ":"JWT"}"#;

type HmacSha256 = Hmac<Sha256>;

/// HMAC-SHA256 of `message` under `secret`.
pub(crate) fn sign(message: &[u8], secret: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

/// Encode `payload_json` under the fixed header and sign it: `h.p.s`.
pub(crate) fn sign_token(payload_json: &[u8], secret: &[u8]) -> String {
    let header = URL_SAFE_NO_PAD.encode(HEADER_JSON);
    let payload = URL_SAFE_NO_PAD.encode(payload_json);
    let signing_input = format!("{header}.{payload}");
    let signature = sign(signing_input.as_bytes(), secret);
    format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature))
}

/// Check the three-segment shape, the exact header, and the signature under
/// `secret` (constant time), then return the decoded payload bytes. The
/// caller still owns every claim check. `None` on any malformed input.
pub(crate) fn verified_payload(token: &str, secret: &[u8]) -> Option<Vec<u8>> {
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
    URL_SAFE_NO_PAD.decode(payload).ok()
}
