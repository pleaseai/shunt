//! Which `thinking` block signatures shunt minted, and which Anthropic issued.
//!
//! An Anthropic thinking block carries an opaque `signature` only
//! `api.anthropic.com` can produce. Every other provider that surfaces
//! reasoning through shunt's Anthropic-protocol surface still has to put
//! *something* there, because Claude Code round-trips whole blocks verbatim and
//! a thinking block is not a thinking block without the field.
//!
//! Three producers do that today, and each one's value is defined here rather
//! than at its own emit site. That is the point of the module: the strip that
//! mirrors them ([`crate::adapters::anthropic::thinking`]) reads these same
//! predicates, so a fourth provider cannot be added without passing through the
//! one file that decides what a foreign signature looks like. Left at their
//! emit sites, the constants would be three unrelated literals and the strip a
//! fourth copy of them — which is how the next provider leaks its signature
//! upstream silently.
//!
//! The asymmetry is deliberate: shunt can recognise its own signatures exactly,
//! and cannot verify Anthropic's at all. So the predicate names what shunt
//! minted and treats everything else as Anthropic's to judge.

/// What `src/model/gemini.rs` puts in a thinking block it synthesises from a
/// Gemini `thought` part. Gemini issues no signature of its own.
pub const GEMINI: &str = "gemini_thinking";

/// What `src/adapters/cursor/sse.rs` opens a thinking block with. The Cursor
/// agent stream carries reasoning text and nothing signature-shaped, so the
/// field exists only to make the block well-formed for the client.
pub const CURSOR: &str = "";

/// True when shunt minted this signature rather than Anthropic.
///
/// Used to decide what must not be forwarded to an Anthropic-protocol upstream.
/// It is the union of every producer above plus the Responses round-trip
/// encoding, and it is deliberately the *wider* half of the pair: a signature
/// this returns `true` for is certainly not Anthropic's, while `false` only
/// means shunt did not recognise it — which is the correct default, since a
/// genuine signature is opaque and unverifiable here.
pub fn is_shunt_minted(signature: &str) -> bool {
    // Cursor's is the empty string, and an empty signature cannot be a valid
    // Anthropic one under any reading, so this arm covers more than Cursor.
    signature == CURSOR
        || signature == GEMINI
        || crate::model::responses_request::is_reasoning_signature(signature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::responses_request::encode_reasoning_signature;

    /// Non-vacuity: this pins a *disjunction*, so a stub returning `true`
    /// satisfies every arm at once — `an_anthropic_signature_is_not_ours` is the
    /// twin that fails it. Drop any single arm from [`is_shunt_minted`] and the
    /// matching case below goes red while the others stay green, so no arm is
    /// carried by its neighbours.
    #[test]
    fn every_producer_shunt_has_is_recognised() {
        let cases: [(&str, String); 3] = [
            ("gemini", GEMINI.to_string()),
            ("cursor", CURSOR.to_string()),
            (
                "responses",
                encode_reasoning_signature("rs_abc", "encrypted-payload"),
            ),
        ];
        for (name, signature) in cases {
            assert!(
                is_shunt_minted(&signature),
                "{name} mints this signature, so the strip must recognise it"
            );
        }
    }

    #[test]
    fn an_anthropic_signature_is_not_ours() {
        // Shape-wise the closest thing to a false positive: long, opaque, and
        // base64-ish, but not shunt's `{id, enc}` payload.
        for signature in [
            "ErUBCkYIBBgCKkBQ0yIvVG9rZW4gc2lnbmF0dXJl",
            "not-base64!!",
            "gemini_thinking_but_longer",
        ] {
            assert!(
                !is_shunt_minted(signature),
                "{signature} is not one shunt minted, so it must be forwarded untouched"
            );
        }
    }

    #[test]
    fn a_base64_payload_missing_either_field_is_not_ours() {
        // `is_reasoning_signature` decodes structurally rather than by shape, so
        // a signature that merely *looks* like ours is not claimed by it.
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        for payload in [r#"{"id":"rs_1"}"#, r#"{"enc":"x"}"#, r#"["rs_1","x"]"#] {
            let signature = URL_SAFE_NO_PAD.encode(payload);
            assert!(
                !is_shunt_minted(&signature),
                "{payload} is not the round-trip payload shunt encodes"
            );
        }
    }
}
