//! Drop thinking blocks whose signature shunt minted rather than Anthropic.
//!
//! Shunt surfaces every provider through the Anthropic Messages protocol, so a
//! turn served by a Responses, Gemini, or Cursor upstream reaches the client as
//! Anthropic-shaped thinking blocks — with a `signature` shunt had to invent,
//! because the field is not optional in a well-formed block
//! ([`crate::model::thinking_signature`]). Claude Code round-trips assistant
//! blocks verbatim, so those signatures come back in `messages` on the next
//! turn. When that next turn resolves to an Anthropic-kind upstream — a manual
//! `/model` switch, a failover, or a `[models.router]` tier flip — the
//! body carries a signature `api.anthropic.com` never issued.
//!
//! The reverse direction is already handled and has been since the Responses
//! translator was written: `decode_reasoning_signature` returns `None` for a
//! genuine Anthropic signature and the block is dropped rather than forwarded,
//! "the Responses backend rejects reasoning it never issued". Nothing did the
//! same on the way back. This module is that mirror.
//!
//! **What it deliberately does not do.** It never touches a signature shunt
//! does not recognise. A genuine signature is opaque and cannot be verified
//! here, so "not ours" is forwarded untouched — dropping a real thinking block
//! would break exactly the extended-thinking tool-use continuation the field
//! exists to carry.

use serde_json::Value;

use crate::model::thinking_signature;
use crate::request::RequestBody;

/// Remove thinking blocks carrying a shunt-minted signature.
pub(super) fn strip_foreign_thinking(body: &mut RequestBody) {
    // Not for byte preservation — `mutate` already skips re-serialising when the
    // closure reports no change. This is about the clone: `Arc::make_mut` runs
    // *before* the closure decides, so without this check every Anthropic
    // request would deep-copy a `messages` tree it shares with routing, to then
    // change nothing. The read-only scan is the cheaper half by far.
    if !request_carries_foreign_thinking(body.json()) {
        return;
    }
    body.mutate(strip_foreign_thinking_blocks);
}

fn request_carries_foreign_thinking(request: &Value) -> bool {
    let Some(messages) = request.get("messages").and_then(Value::as_array) else {
        return false;
    };
    messages.iter().any(|message| {
        message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|content| content.iter().any(is_foreign_thinking_block))
    })
}

fn strip_foreign_thinking_blocks(request: &mut Value) -> bool {
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return false;
    };
    // An assistant turn whose blocks are *all* foreign thinking leaves whole.
    // Emptying its `content` is not an option — Anthropic rejects an empty array
    // — and this is not the degenerate shape it looks like: `responses.rs` opens
    // an empty thinking block purely to carry the round-trip signature, so a
    // reasoning-only turn is a shape the Responses path routinely produces
    // (`reasoning_only_turn_does_not_emit_empty_text_block`). Dropping the
    // message is what the rest of this codebase already does with one:
    // `inbound_responses::messages_request`'s `flush` drops any assistant turn
    // left holding nothing but thinking, because "Anthropic rejects an assistant
    // message holding nothing but a thinking block" — at any position, not only
    // a trailing one.
    let before_messages = messages.len();
    messages.retain(|message| !is_all_foreign_assistant_turn(message));
    let mut changed = messages.len() != before_messages;
    for message in messages {
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        let before = content.len();
        content.retain(|block| !is_foreign_thinking_block(block));
        changed |= content.len() != before;
    }
    changed
}

/// True for an assistant message that would be emptied by the strip.
///
/// Restricted to `assistant` because that is the only role whose turn can be
/// reconstructed from what survives it; a `user` message is the client's own
/// content and is never dropped, however odd its blocks look. An empty `content`
/// array is left alone too — `all` is vacuously true there, and an already-empty
/// message is not this pass's to fix.
fn is_all_foreign_assistant_turn(message: &Value) -> bool {
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return false;
    }
    message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|content| !content.is_empty() && content.iter().all(is_foreign_thinking_block))
}

/// True for a thinking block that `api.anthropic.com` cannot have issued.
///
/// A block with no `signature` at all counts: Anthropic always issues one, so
/// its absence means the block did not come from there either. Both halves are
/// the wide side of the pair on purpose — this predicate only ever removes
/// blocks that are already invalid upstream.
fn is_foreign_thinking_block(block: &Value) -> bool {
    if block.get("type").and_then(Value::as_str) != Some("thinking") {
        return false;
    }
    block
        .get("signature")
        .and_then(Value::as_str)
        .is_none_or(thinking_signature::is_shunt_minted)
}

#[cfg(test)]
mod tests {
    //! Non-vacuity. Every assertion here that a block *leaves* is satisfied by a
    //! pass that removes every thinking block, so each is paired:
    //! `an_anthropic_signature_survives_the_strip` is the twin for the four drop
    //! cases — make [`is_foreign_thinking_block`] return `true` for every
    //! thinking block and it goes red, along with
    //! `only_the_foreign_blocks_leave_and_other_turns_are_untouched`.
    //!
    //! `an_all_foreign_assistant_turn_is_dropped_whole` pins the message-level
    //! drop; remove the `retain` over `messages` and it goes red. Its twins are
    //! `a_genuine_thinking_only_turn_is_not_dropped` (mutate the predicate to
    //! match any thinking-only assistant turn) and `a_user_turn_is_never_dropped_whole`
    //! (drop the `assistant` role check) — each mutation reds exactly one, so no
    //! twin is carried by its neighbours. An earlier revision of this module left
    //! that turn in place and had a test asserting so; it was codifying the leak,
    //! since a reasoning-only turn is exactly what `responses.rs` emits when it
    //! opens an empty thinking block to carry a signature.
    //!
    //! One trap, measured the hard way: a test whose request contains *no*
    //! foreign block never reaches the strip at all, because
    //! `request_carries_foreign_thinking` returns first. The first version of
    //! `a_genuine_thinking_only_turn_is_not_dropped` was that shape and stayed
    //! green under its own mutation. It now carries a foreign block in a later
    //! turn purely to keep the pass running. Any test added here that asserts
    //! something *survives* needs the same, or it asserts nothing.
    //!
    //! `a_body_with_nothing_to_strip_keeps_its_exact_bytes` is the odd one, and
    //! the honest statement is that **neither** single mutation turns it red:
    //! deleting the precheck leaves [`RequestBody::mutate`] to skip the
    //! re-serialisation on a `false` return, and breaking that `false` return
    //! leaves the precheck to return before the closure ever runs. Two
    //! independent mechanisms hold the same property, so only removing both
    //! together fails it — which was measured rather than reasoned about, after
    //! the first version of this paragraph claimed the precheck alone carried
    //! it. The useful consequence for a later reader: the precheck is a
    //! clone-avoidance, and dropping it costs a deep copy per request, not
    //! correctness.

    use serde_json::json;

    use super::*;
    use crate::model::responses_request::encode_reasoning_signature;

    fn body(request: serde_json::Value) -> RequestBody {
        RequestBody::parse(serde_json::to_vec(&request).expect("test fixture serialises"))
            .expect("test fixture parses")
    }

    fn assistant_turn(signature: Option<&str>) -> serde_json::Value {
        let mut thinking = json!({"type": "thinking", "thinking": "weighing the options"});
        if let Some(signature) = signature {
            thinking["signature"] = json!(signature);
        }
        json!({
            "role": "assistant",
            "content": [thinking, {"type": "text", "text": "here is the plan"}]
        })
    }

    fn content_types(request: &Value, message: usize) -> Vec<String> {
        request["messages"][message]["content"]
            .as_array()
            .expect("content is an array")
            .iter()
            .map(|block| block["type"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    #[test]
    fn every_shunt_minted_signature_is_dropped() {
        let responses = encode_reasoning_signature("rs_abc", "encrypted-payload");
        let cases: [(&str, Option<&str>); 4] = [
            ("responses", Some(responses.as_str())),
            ("gemini", Some(thinking_signature::GEMINI)),
            ("cursor", Some(thinking_signature::CURSOR)),
            ("absent", None),
        ];
        for (name, signature) in cases {
            let mut request = body(json!({
                "model": "claude-opus-4-8",
                "messages": [assistant_turn(signature)],
            }));
            strip_foreign_thinking(&mut request);
            assert_eq!(
                content_types(request.json(), 0),
                vec!["text"],
                "a {name} signature is not Anthropic's, so the block must not be forwarded"
            );
        }
    }

    #[test]
    fn an_anthropic_signature_survives_the_strip() {
        let mut request = body(json!({
            "model": "claude-opus-4-8",
            "messages": [assistant_turn(Some("ErUBCkYIBBgCKkBQ0yIvVG9rZW4"))],
        }));
        strip_foreign_thinking(&mut request);
        assert_eq!(
            content_types(request.json(), 0),
            vec!["thinking", "text"],
            "an unrecognised signature is Anthropic's to judge, not shunt's to drop"
        );
    }

    #[test]
    fn a_body_with_nothing_to_strip_keeps_its_exact_bytes() {
        // Key order and spacing that `serde_json::to_vec` would not reproduce, so
        // a needless re-serialisation is visible rather than coincidentally equal.
        let raw =
            br#"{ "messages" : [{"role":"user","content":"hi"}], "model":"claude-opus-4-8" }"#;
        let mut request = RequestBody::parse(raw.to_vec()).expect("fixture parses");
        strip_foreign_thinking(&mut request);
        assert_eq!(
            request.into_raw(),
            raw.to_vec(),
            "a conversation with no foreign block must pass through byte-for-byte"
        );
    }

    /// The case this pass used to skip, and the one the Responses path produces
    /// most readily: `responses.rs` opens an empty thinking block purely to carry
    /// the round-trip signature, so an assistant turn can consist of nothing else.
    /// Leaving it forwards the exact signature this module exists to remove.
    #[test]
    fn an_all_foreign_assistant_turn_is_dropped_whole() {
        let mut request = body(json!({
            "model": "claude-opus-4-8",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "go"}]},
                {
                    "role": "assistant",
                    "content": [{
                        "type": "thinking",
                        "thinking": "nothing else in this turn",
                        "signature": thinking_signature::GEMINI,
                    }],
                },
                {"role": "user", "content": [{"type": "text", "text": "still there?"}]},
            ],
        }));
        strip_foreign_thinking(&mut request);
        let messages = request.json()["messages"]
            .as_array()
            .expect("messages")
            .clone();
        assert_eq!(
            messages.len(),
            2,
            "an assistant turn holding nothing but a foreign thinking block has to \
             leave with it — emptying its `content` is rejected upstream, and \
             `inbound_responses::messages_request` drops the same shape"
        );
        assert!(
            messages.iter().all(|message| message["role"] == "user"),
            "the surviving messages are the client's own turns"
        );
    }

    #[test]
    fn a_genuine_thinking_only_turn_is_not_dropped() {
        let mut request = body(json!({
            "model": "claude-opus-4-8",
            "messages": [
                {
                    "role": "assistant",
                    "content": [{
                        "type": "thinking",
                        "thinking": "Anthropic's own",
                        "signature": "ErUBCkYIBBgCKkBQ0yIvVG9rZW4",
                    }],
                },
                {"role": "user", "content": [{"type": "text", "text": "carry on"}]},
                // Present only so the pass does not return early. Without it
                // `request_carries_foreign_thinking` short-circuits and this test
                // holds under any predicate at all — that was measured, not
                // assumed: the earlier single-message version stayed green while
                // the predicate was mutated to drop every thinking-only turn.
                assistant_turn(Some(thinking_signature::GEMINI)),
            ],
        }));
        strip_foreign_thinking(&mut request);
        assert_eq!(
            request.json()["messages"]
                .as_array()
                .expect("messages")
                .len(),
            3,
            "the message-level drop is keyed on the signature, not on the shape: a \
             thinking-only turn Anthropic signed stays"
        );
        assert_eq!(
            content_types(request.json(), 0),
            vec!["thinking"],
            "and its block is untouched"
        );
        assert_eq!(
            content_types(request.json(), 2),
            vec!["text"],
            "while the foreign block in the turn that kept the pass running did leave"
        );
    }

    #[test]
    fn a_user_turn_is_never_dropped_whole() {
        // A `user` message carrying only a foreign-looking thinking block is not a
        // turn this pass can reconstruct, and losing it would lose the client's
        // own content — so the message-level drop is assistant-only.
        let mut request = body(json!({
            "model": "claude-opus-4-8",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "thinking",
                    "thinking": "odd but the client's",
                    "signature": thinking_signature::GEMINI,
                }],
            }],
        }));
        strip_foreign_thinking(&mut request);
        assert_eq!(
            request.json()["messages"]
                .as_array()
                .expect("messages")
                .len(),
            1,
            "only an assistant turn is dropped whole"
        );
    }

    #[test]
    fn only_the_foreign_blocks_leave_and_other_turns_are_untouched() {
        let mut request = body(json!({
            "model": "claude-opus-4-8",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "go"}]},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "a", "signature": thinking_signature::GEMINI},
                        {"type": "thinking", "thinking": "b", "signature": "ErUBCkYIBBgCKkB"},
                        {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {}},
                    ],
                },
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1"}]},
            ],
        }));
        strip_foreign_thinking(&mut request);
        let request = request.json();
        assert_eq!(content_types(request, 0), vec!["text"]);
        assert_eq!(
            content_types(request, 1),
            vec!["thinking", "tool_use"],
            "the surviving thinking block is the one shunt did not mint"
        );
        assert_eq!(
            request["messages"][1]["content"][0]["thinking"], "b",
            "and it is the *right* one — a pass keyed on position rather than on \
             the signature would keep block `a` here"
        );
        assert_eq!(content_types(request, 2), vec!["tool_result"]);
    }

    #[test]
    fn a_string_content_message_is_not_disturbed() {
        let mut request = body(json!({
            "model": "claude-opus-4-8",
            "messages": [
                {"role": "user", "content": "plain string content"},
                assistant_turn(Some(thinking_signature::GEMINI)),
            ],
        }));
        strip_foreign_thinking(&mut request);
        assert_eq!(
            request.json()["messages"][0]["content"],
            "plain string content"
        );
        assert_eq!(content_types(request.json(), 1), vec!["text"]);
    }
}
