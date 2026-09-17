//! Drop thinking blocks whose signature shunt minted rather than Anthropic.
//!
//! Shunt surfaces every provider through the Anthropic Messages protocol, so a
//! turn served by a Responses, Gemini, or Cursor upstream reaches the client as
//! Anthropic-shaped thinking blocks — with a `signature` shunt had to invent,
//! because the field is not optional in a well-formed block
//! ([`crate::model::thinking_signature`]). Claude Code round-trips assistant
//! blocks verbatim, so those signatures come back in `messages` on the next
//! turn. When that next turn resolves to an Anthropic-kind upstream — a manual
//! `/model` switch, a failover, or a `[models.stage_router]` tier flip — the
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
    let mut changed = false;
    for message in messages {
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        if content.iter().all(is_foreign_thinking_block) {
            // Every block would go. Anthropic rejects an empty `content` array,
            // dropping the message would break user/assistant alternation, and
            // inventing a replacement block would put words in the assistant's
            // mouth — there is no local transform that makes this message valid,
            // which is the same conclusion `proxy::normalize_empty_text_blocks`
            // reaches about an all-empty-text message. Leave it: a turn that was
            // *only* reasoning, with no text and no `tool_use`, fails upstream
            // whether or not this pass touches it.
            //
            // Note this arm is also what `all` returns for an empty array, which
            // is the right answer for one of those too.
            continue;
        }
        let before = content.len();
        content.retain(|block| !is_foreign_thinking_block(block));
        changed |= content.len() != before;
    }
    changed
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
    //! `a_reasoning_only_turn_is_left_alone` pins the one case this pass
    //! declines to fix; drop its `all` guard and it goes red with an empty
    //! `content` array, which is what the guard exists to avoid producing.
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

    #[test]
    fn a_reasoning_only_turn_is_left_alone() {
        let mut request = body(json!({
            "model": "claude-opus-4-8",
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "thinking",
                    "thinking": "nothing else in this turn",
                    "signature": thinking_signature::GEMINI,
                }],
            }],
        }));
        strip_foreign_thinking(&mut request);
        assert_eq!(
            content_types(request.json(), 0),
            vec!["thinking"],
            "emptying the array would be rejected upstream too, so this pass declines it"
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
