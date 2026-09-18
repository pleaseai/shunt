//! Round-trip an Anthropic `thinking` block through a Responses `reasoning`
//! item's `encrypted_content`.
//!
//! The Codex CLI runs `store: false` and replays every output item — reasoning
//! included — as input on the next turn. Anthropic only accepts a `thinking`
//! block back when it carries the exact `signature` it issued, so the
//! signature has to survive that round trip inside a field the CLI treats as
//! opaque. `encrypted_content` is that field: [`encode_thinking`] packs the
//! thinking text and its signature into it, and [`decode_thinking`] unpacks it
//! on the way back. A `reasoning` item whose `encrypted_content` shunt did not
//! produce (a genuine OpenAI one, say) decodes to `None` and is dropped, since
//! Anthropic would reject a thinking block it never signed.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};

/// The Anthropic content block a Responses `reasoning` item round-trips.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Thinking {
    /// A visible `thinking` block: text plus the signature Anthropic issued.
    Signed { thinking: String, signature: String },
    /// A `redacted_thinking` block: opaque `data` only.
    Redacted { data: String },
}

/// Pack a thinking block into the opaque `encrypted_content` of a Responses
/// `reasoning` output item.
pub fn encode_thinking(block: &Thinking) -> String {
    let payload = match block {
        Thinking::Signed {
            thinking,
            signature,
        } => json!({"t": thinking, "s": signature}),
        Thinking::Redacted { data } => json!({"r": data}),
    };
    URL_SAFE_NO_PAD.encode(payload.to_string())
}

/// Inverse of [`encode_thinking`]. `None` for content shunt did not produce.
pub fn decode_thinking(encrypted_content: &str) -> Option<Thinking> {
    let bytes = URL_SAFE_NO_PAD.decode(encrypted_content).ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let field = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    if let Some(data) = field("r") {
        return Some(Thinking::Redacted { data });
    }
    Some(Thinking::Signed {
        thinking: field("t")?,
        signature: field("s")?,
    })
}

/// Turn a decoded [`Thinking`] back into the Anthropic content block it came
/// from, ready to be placed in an assistant message.
pub fn thinking_block(block: Thinking) -> Value {
    match block {
        Thinking::Signed {
            thinking,
            signature,
        } => json!({"type": "thinking", "thinking": thinking, "signature": signature}),
        Thinking::Redacted { data } => json!({"type": "redacted_thinking", "data": data}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_thinking_round_trips_text_and_signature() {
        let block = Thinking::Signed {
            thinking: "consider the edge case".to_string(),
            signature: "sig-abc".to_string(),
        };
        assert_eq!(decode_thinking(&encode_thinking(&block)), Some(block));
    }

    #[test]
    fn redacted_thinking_round_trips_its_data() {
        let block = Thinking::Redacted {
            data: "opaque".to_string(),
        };
        assert_eq!(decode_thinking(&encode_thinking(&block)), Some(block));
    }

    #[test]
    fn foreign_encrypted_content_decodes_to_none() {
        assert_eq!(decode_thinking("gAAAAABnot-shunt"), None);
        assert_eq!(
            decode_thinking(&URL_SAFE_NO_PAD.encode(r#"{"id":"rs_1","enc":"x"}"#)),
            None
        );
    }

    #[test]
    fn thinking_block_rebuilds_the_anthropic_shape() {
        let signed = thinking_block(Thinking::Signed {
            thinking: "t".to_string(),
            signature: "s".to_string(),
        });
        assert_eq!(
            signed,
            json!({"type": "thinking", "thinking": "t", "signature": "s"})
        );
        let redacted = thinking_block(Thinking::Redacted {
            data: "d".to_string(),
        });
        assert_eq!(redacted, json!({"type": "redacted_thinking", "data": "d"}));
    }
}
