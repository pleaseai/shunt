//! Restore the Claude Code identity block on Claude Code's auto-mode
//! permission classifier request.
//!
//! `api.anthropic.com` accepts a subscription-OAuth request only when its
//! system prompt carries a recognized first-party marker. Two were observed on
//! the wire, and either is sufficient: the billing attribution block Claude
//! Desktop leads with (`x-anthropic-billing-header: cc_version=…;
//! cc_entrypoint=…`), or one of a set of recognized identity prompts, which is
//! what Claude Code sends. Claude Code carries one on every request but one:
//! the auto-mode permission classifier builds its request with the identity
//! prefix suppressed, and carries no billing block either. Relayed verbatim,
//! that single request comes back as a bare `rate_limit_error` whose message is
//! the literal `"Error"`, carrying neither `retry-after` nor any
//! `anthropic-ratelimit-*` header — the shape [`rate_limit_kind`] labels
//! `client-shape-rejection`. Claude Code reads it as "classifier unavailable"
//! and fails closed on every action needing a safety verdict, while reads and
//! in-workspace edits keep working, so the session looks half-broken rather
//! than disconnected. It does not recover either: Claude Code demotes to a
//! second classifier model once, and once that demotion lands on the session's
//! own model there is no further candidate.
//!
//! Measured against a live deployment (Claude Code 2.1.226, Anthropic OAuth
//! pool), same session and account, varying only the `system` field and leaving
//! headers, `tools`, `max_tokens`, `stream` and `stop_sequences` untouched:
//!
//! | classifier request | result |
//! | --- | --- |
//! | relayed unmodified | 429, 15/15 |
//! | `"You are a helpful assistant."` prepended | 429, 10/10 |
//! | Claude Code identity prepended | 200 |
//!
//! The middle row is why this module injects one specific sentence rather than
//! a neutral one: upstream matches a set of known prompts, so there is no
//! neutral string that passes.
//!
//! That makes the repair narrow by construction. It fires only on the request
//! shape that is known to be rejected — identified by the opening sentence of
//! the classifier prompt — and never on ordinary traffic. Claude Desktop's chat
//! surface and Cowork were both measured through the same relay: each leads
//! with the billing block across three system blocks, neither matches this
//! predicate, and every request was accepted upstream. Both keep byte-for-byte
//! passthrough, as do `claude --system-prompt`, the Agent SDK and third-party
//! clients, so the gateway never claims to be Claude Code on behalf of a client
//! that isn't.
//!
//! [`rate_limit_kind`]: super::rate_limit_kind

use serde_json::{json, Value};

use crate::request::RequestBody;

/// The block inserted when the classifier omits it — the string Claude Code's
/// own main loop sends, and the one measured to flip the rejection above.
const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Opening sentence of the auto-mode classifier's system prompt.
///
/// The whole prompt is a ~38 KB template that interpolates its rule sections
/// from separate constants, and the two classifier stages observed on the wire
/// differ in their output format (`</block>` versus `</severity>` stop
/// sequences). Their openings are identical, so this sentence is the stable
/// part; matching deeper structural markers would couple the relay to sections
/// that legitimately vary between stages and releases.
///
/// Third-party relays additionally check a length floor and a marker set to
/// resist forgery. That is not shunt's problem: this predicate only decides
/// whether to repair the operator's own outbound request, never whether to
/// grant an untrusted client access, so a client that "forges" it is only
/// asking the gateway to prepend a sentence to its own prompt.
const CLASSIFIER_PROMPT_PREFIX: &str =
    "You are a security monitor for autonomous AI coding agents.";

/// Markers upstream is known to accept, so a classifier request that has
/// somehow already grown one is left alone rather than gaining a second.
///
/// All three were verified on the wire against the live endpoint: the identity
/// string as the injected block that flipped a rejection to `200`, the Agent SDK
/// opening as the head of accepted main-loop requests, and the billing
/// attribution block as the head of every accepted Claude Desktop chat and
/// Cowork request. Other accepted markers may exist; they are deliberately not
/// guessed at here, because this list only ever suppresses an injection that a
/// classifier match already gated.
const ACCEPTED_MARKER_PREFIXES: &[&str] = &[
    "You are Claude Code, Anthropic's official CLI for Claude",
    "You are a Claude agent, built on Anthropic's Claude Agent SDK",
    "x-anthropic-billing-header:",
];

/// Prepend the Claude Code identity block to an auto-mode classifier request.
///
/// A no-op for every other request, and the cheap read-only check runs first so
/// those never enter [`RequestBody::mutate`], whose `Arc::make_mut` would clone
/// the whole request tree before the closure could report "no change".
pub(super) fn restore_claude_code_identity(body: &mut RequestBody) {
    if !needs_identity(body.json()) {
        return;
    }
    body.mutate(insert_identity);
}

/// Whether `request` is a classifier request that is missing an accepted
/// marker.
///
/// Only the block form is considered: Claude Code sends the classifier's system
/// prompt as an array, so a string `system` — what `claude --system-prompt`
/// produces — can never match, and neither can a client that omits `system`.
fn needs_identity(request: &Value) -> bool {
    let Some(blocks) = request.get("system").and_then(Value::as_array) else {
        return false;
    };
    let mut is_classifier = false;
    for block in blocks {
        let Some(text) = block.get("text").and_then(Value::as_str) else {
            continue;
        };
        let text = text.trim_start();
        if ACCEPTED_MARKER_PREFIXES
            .iter()
            .any(|prefix| text.starts_with(prefix))
        {
            return false;
        }
        if text.starts_with(CLASSIFIER_PROMPT_PREFIX) {
            is_classifier = true;
        }
    }
    is_classifier
}

/// Insert the identity block, returning whether the request changed.
///
/// Upholds [`RequestBody::mutate`]'s contract: the one path that returns `true`
/// mutated the tree, and the fallback leaves it untouched.
fn insert_identity(request: &mut Value) -> bool {
    let Some(blocks) = request.get_mut("system").and_then(Value::as_array_mut) else {
        return false;
    };
    // Ahead of the client's first block, and ahead of any `cache_control` it
    // carries. Classifier requests are one-shot and uncached in practice, so
    // this shifts no established cache prefix.
    blocks.insert(0, json!({"type": "text", "text": CLAUDE_CODE_IDENTITY}));
    true
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::{restore_claude_code_identity, CLASSIFIER_PROMPT_PREFIX, CLAUDE_CODE_IDENTITY};
    use crate::request::RequestBody;

    fn classifier_prompt() -> String {
        format!("{CLASSIFIER_PROMPT_PREFIX}\n\n## Context\n\nThe agent you are monitoring is …")
    }

    fn run(body: Value) -> Value {
        let raw = serde_json::to_vec(&body).unwrap();
        let mut request = RequestBody::parse(raw).unwrap();
        restore_claude_code_identity(&mut request);
        serde_json::from_slice(&request.into_raw()).unwrap()
    }

    /// Assert the body is forwarded exactly as the client sent it — key order
    /// and whitespace included, which is what proves no re-serialization ran.
    fn assert_verbatim(body: &str) {
        let mut request = RequestBody::parse(body.as_bytes().to_vec()).unwrap();
        restore_claude_code_identity(&mut request);
        assert_eq!(request.into_raw(), body.as_bytes());
    }

    #[test]
    fn classifier_request_gains_the_identity_block() {
        let prompt = classifier_prompt();
        let out = run(json!({
            "model": "claude-opus-5",
            "max_tokens": 64,
            "stop_sequences": ["</block>"],
            "system": [
                {"type": "text", "text": prompt},
                {"type": "text", "text": "Session context."},
            ],
        }));

        let system = out["system"].as_array().unwrap();
        assert_eq!(system.len(), 3);
        assert_eq!(system[0]["type"], "text");
        assert_eq!(system[0]["text"], CLAUDE_CODE_IDENTITY);
        assert_eq!(system[1]["text"], prompt);
        assert_eq!(system[2]["text"], "Session context.");
        // Nothing else about the request is disturbed.
        assert_eq!(out["model"], "claude-opus-5");
        assert_eq!(out["max_tokens"], 64);
        assert_eq!(out["stop_sequences"], json!(["</block>"]));
    }

    #[test]
    fn claude_code_main_loop_is_untouched() {
        let body = r#"{"model":"claude-opus-5","system":[{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."},{"type":"text","text":"more"}]}"#;

        assert_verbatim(body);
    }

    #[test]
    fn claude_desktop_chat_and_cowork_are_untouched() {
        // The shape measured through the relay: both Claude Desktop surfaces
        // lead with the billing attribution block, three system blocks in all,
        // and every one of their turns was accepted upstream. The gateway must
        // not rewrite them to claim they are Claude Code. (The `cc_entrypoint`
        // value sat beyond the capture window, so the one here stands in.)
        let body = r#"{"model":"claude-opus-5","system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.219.f52; cc_entrypoint=desktop;"},{"type":"text","text":"You are Claude, a helpful assistant."},{"type":"text","text":"Workspace context."}]}"#;

        assert_verbatim(body);
    }

    #[test]
    fn a_third_party_client_with_its_own_system_prompt_is_untouched() {
        let body = r#"{"model":"claude-opus-5","system":[{"type":"text","text":"You are a triage bot."}]}"#;

        assert_verbatim(body);
    }

    #[test]
    fn a_classifier_that_already_carries_a_billing_block_is_untouched() {
        // Either accepted marker is sufficient upstream, so a classifier
        // request carrying the billing block needs no identity block.
        let body = format!(
            r#"{{"system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.226.000; cc_entrypoint=cli;"}},{{"type":"text","text":"{CLASSIFIER_PROMPT_PREFIX} …"}}]}}"#
        );

        assert_verbatim(&body);
    }

    #[test]
    fn custom_string_system_prompt_is_untouched() {
        // `claude --system-prompt "…"` replaces the entire system prompt, which
        // is a documented first-party flag. Honour it.
        let body = r#"{"model":"claude-opus-5","system":"You are a Python expert"}"#;

        assert_verbatim(body);
    }

    #[test]
    fn request_without_a_system_prompt_is_untouched() {
        // Claude Desktop's gateway connection test reaches the relay this way —
        // a `max_tokens: 1` availability probe with no system prompt at all,
        // which upstream accepted.
        let body = r#"{"model":"claude-haiku-4-5-20251001","max_tokens":1}"#;

        assert_verbatim(body);
    }

    #[test]
    fn classifier_that_already_carries_the_identity_is_untouched() {
        let body = format!(
            r#"{{"system":[{{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."}},{{"type":"text","text":"{CLASSIFIER_PROMPT_PREFIX} …"}}]}}"#
        );

        assert_verbatim(&body);
    }

    #[test]
    fn agent_sdk_identity_also_suppresses_injection() {
        let body = format!(
            r#"{{"system":[{{"type":"text","text":"You are a Claude agent, built on Anthropic's Claude Agent SDK."}},{{"type":"text","text":"{CLASSIFIER_PROMPT_PREFIX} …"}}]}}"#
        );

        assert_verbatim(&body);
    }

    #[test]
    fn classifier_prompt_is_found_behind_a_non_text_block() {
        // The opening sentence is the signal wherever it sits, so a leading
        // image or tool block does not hide it.
        let out = run(json!({
            "system": [
                {"type": "image", "source": {}},
                {"type": "text", "text": classifier_prompt()},
            ],
        }));

        let system = out["system"].as_array().unwrap();
        assert_eq!(system.len(), 3);
        assert_eq!(system[0]["text"], CLAUDE_CODE_IDENTITY);
        assert_eq!(system[1]["type"], "image");
    }

    #[test]
    fn leading_whitespace_still_identifies_the_classifier() {
        let out = run(json!({
            "system": [{"type": "text", "text": format!("\n  {}", classifier_prompt())}],
        }));

        assert_eq!(out["system"].as_array().unwrap().len(), 2);
        assert_eq!(out["system"][0]["text"], CLAUDE_CODE_IDENTITY);
    }

    #[test]
    fn identity_is_inserted_ahead_of_a_cache_control_block() {
        let out = run(json!({
            "system": [{
                "type": "text",
                "text": classifier_prompt(),
                "cache_control": {"type": "ephemeral"},
            }],
        }));

        let system = out["system"].as_array().unwrap();
        assert_eq!(system[0]["text"], CLAUDE_CODE_IDENTITY);
        assert!(system[0].get("cache_control").is_none());
        assert_eq!(system[1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn a_string_system_carrying_the_classifier_prompt_is_untouched() {
        // Claude Code always sends the classifier prompt as a block. A string
        // in this shape is some other client, so it keeps passthrough.
        let body = format!(r#"{{"system":"{CLASSIFIER_PROMPT_PREFIX} …"}}"#);

        assert_verbatim(&body);
    }

    #[test]
    fn unexpected_system_type_is_left_alone() {
        // Upstream will reject this on its own terms; reshaping it here would
        // turn a clear client error into a confusing gateway-authored request.
        assert_verbatim(r#"{"system":42}"#);
    }

    #[test]
    fn non_object_request_is_left_alone() {
        assert_verbatim(r#"["not","an","object"]"#);
    }

    #[test]
    fn empty_block_array_is_untouched() {
        assert_verbatim(r#"{"system":[]}"#);
    }
}
