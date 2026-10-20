//! Which `thinking` block signatures shunt minted, and which Anthropic issued.
//!
//! An Anthropic thinking block carries an opaque `signature` only
//! `api.anthropic.com` can produce. Every other provider that surfaces
//! reasoning through shunt's Anthropic-protocol surface still has to put
//! *something* there, because Claude Code round-trips whole blocks verbatim and
//! a thinking block is not a thinking block without the field.
//!
//! Three producers mint a value of their own today, and a fourth form (below)
//! wraps one shunt did not mint. Each one's value is defined here rather than
//! at its own emit site. That is the point of the module: the strip that
//! mirrors them ([`crate::adapters::anthropic::thinking`]) reads these same
//! predicates, so a new provider cannot be added without passing through the
//! one file that decides what a foreign signature looks like. Left at their
//! emit sites, the constants would be unrelated literals and the strip another
//! copy of them — which is how the next provider leaks its signature
//! upstream silently.
//!
//! The asymmetry is deliberate: shunt can recognise its own signatures exactly,
//! and cannot verify Anthropic's at all. So the predicate names what shunt
//! minted and treats everything else as Anthropic's to judge.
//!
//! **One form wraps a signature shunt did not mint.** A third-party
//! Anthropic-kind upstream (MiniMax, Kimi, Zhipu, … — anything not on
//! `api.anthropic.com`) is relayed untranslated, so its thinking blocks carry
//! a signature only *that* host can verify, and nothing about the value says
//! so. Left alone it is indistinguishable from Anthropic's own and would be
//! forwarded to `api.anthropic.com` on a later turn, to be rejected with a 400
//! that does not advance the failover chain (issue #783). So the Anthropic
//! adapter wraps such a signature in an envelope naming (by digest) the upstream that
//! issued it ([`wrap_upstream_signature`]); the strip recognises the envelope
//! like any other shunt-minted form, and the adapter unwraps it again only for
//! a request going back to that same upstream.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

/// What `src/model/gemini.rs` puts in a thinking block it synthesises from a
/// Gemini `thought` part. Gemini issues no signature of its own.
pub const GEMINI: &str = "gemini_thinking";

/// What `src/adapters/cursor/sse.rs` opens a thinking block with. The Cursor
/// agent stream carries reasoning text and nothing signature-shaped, so the
/// field exists only to make the block well-formed for the client.
pub const CURSOR: &str = "";

/// Wrap a signature a third-party Anthropic-kind upstream issued, naming that
/// upstream, so it can be told apart from Anthropic's and restored for the
/// host that can verify it.
///
/// `upstream` is a digest of the provider's normalized `base_url`, not its
/// config name and not the URL: the same host is the same signing authority,
/// so the envelope survives a provider rename on reload, and two providers
/// pointing at one `base_url` share signatures the way Anthropic's are valid
/// across pooled accounts. It is a digest because the envelope is
/// client-visible and persists in transcripts, while a `base_url` can carry
/// credentials (userinfo, a path token).
///
/// Same encoding as [`encode_reasoning_signature`] — base64url-no-pad JSON —
/// but with disjoint field names (`upstream`/`sig` against `id`/`enc`), so
/// neither decoder accepts the other's form.
///
/// [`encode_reasoning_signature`]: crate::model::responses_request::encode_reasoning_signature
pub fn wrap_upstream_signature(upstream: &str, signature: &str) -> String {
    let payload = serde_json::json!({"upstream": upstream, "sig": signature});
    URL_SAFE_NO_PAD.encode(payload.to_string())
}

/// Inverse of [`wrap_upstream_signature`]: the `(upstream, signature)` pair,
/// or `None` for anything that envelope did not produce.
pub fn unwrap_upstream_signature(signature: &str) -> Option<(String, String)> {
    let bytes = URL_SAFE_NO_PAD.decode(signature).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let upstream = value.get("upstream")?.as_str()?.to_string();
    let signature = value.get("sig")?.as_str()?.to_string();
    Some((upstream, signature))
}

/// True when shunt minted this signature rather than Anthropic.
///
/// Used to decide what must not be forwarded to an Anthropic-protocol upstream.
/// It is the union of every producer above plus the Responses round-trip
/// encoding and the third-party upstream envelope, and it is deliberately the
/// *wider* half of the pair: a signature this returns `true` for is certainly
/// not Anthropic's, while `false` only means shunt did not recognise it — which is the correct default, since a
/// genuine signature is opaque and unverifiable here.
pub fn is_shunt_minted(signature: &str) -> bool {
    // Cursor's is the empty string, and an empty signature cannot be a valid
    // Anthropic one under any reading, so this arm covers more than Cursor.
    signature == CURSOR
        || signature == GEMINI
        || crate::model::responses_request::is_reasoning_signature(signature)
        || unwrap_upstream_signature(signature).is_some()
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
        let cases: [(&str, String); 4] = [
            ("gemini", GEMINI.to_string()),
            ("cursor", CURSOR.to_string()),
            (
                "responses",
                encode_reasoning_signature("rs_abc", "encrypted-payload"),
            ),
            (
                "third-party envelope",
                wrap_upstream_signature("https://api.minimax.cn/anthropic", "mm-sig"),
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

    #[test]
    fn an_upstream_envelope_round_trips_byte_for_byte() {
        // A signature shaped like nothing in particular — padding, `+`, `/` —
        // so a decoder that normalised it would be visible here.
        let original = "Eq0BCkgIBhABGAIiQ+/=";
        let wrapped = wrap_upstream_signature("https://api.minimax.cn/anthropic", original);
        assert_eq!(
            unwrap_upstream_signature(&wrapped),
            Some((
                "https://api.minimax.cn/anthropic".to_string(),
                original.to_string()
            ))
        );
    }

    /// The two base64url-JSON forms share an encoding, so each decoder must be
    /// keyed on its own field names: a Responses decoder that accepted the
    /// envelope would turn a MiniMax signature into a bogus `reasoning` item,
    /// and the reverse would hand Responses' encrypted content to a third-party
    /// host as if it were that host's signature.
    #[test]
    fn the_envelope_and_the_responses_form_do_not_decode_as_each_other() {
        use crate::model::responses_request::is_reasoning_signature;
        let envelope = wrap_upstream_signature("https://api.minimax.cn/anthropic", "mm-sig");
        let responses = encode_reasoning_signature("rs_abc", "encrypted-payload");
        assert!(
            !is_reasoning_signature(&envelope),
            "the Responses decoder must reject the upstream envelope"
        );
        assert!(
            unwrap_upstream_signature(&responses).is_none(),
            "the envelope decoder must reject the Responses form"
        );
        // Positive twins, so a decoder that rejects everything fails above.
        assert!(is_reasoning_signature(&responses));
        assert!(unwrap_upstream_signature(&envelope).is_some());
    }
}
