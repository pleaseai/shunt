//! Restore the Claude Code identity block on Claude Code requests that reach
//! a subscription-OAuth Anthropic bearer without a first-party marker — the
//! auto-mode permission classifier first among them.
//!
//! `api.anthropic.com` accepts a subscription-OAuth request only when its
//! system prompt carries a recognized first-party marker. Two were observed on
//! the wire, and either is sufficient: the billing attribution block Claude
//! Code and Claude Desktop lead with (`x-anthropic-billing-header:
//! cc_version=…; cc_entrypoint=…`), or one of a set of recognized identity
//! prompts. Claude Code's main loop always carries the identity, but its side
//! requests do not. The auto-mode permission classifier builds its
//! request with the identity prefix suppressed and carries no billing block
//! either. A mod's `$.model.complete` side query sends only the attribution
//! block and the mod's own `system` — and with
//! `CLAUDE_CODE_ATTRIBUTION_HEADER=0`, which the running guide recommends, the
//! attribution block goes too. Relayed verbatim, such a request comes back as a
//! bare `rate_limit_error` whose message is the literal `"Error"`, carrying
//! neither `retry-after` nor any `anthropic-ratelimit-*` header — the shape
//! [`rate_limit_kind`] labels `client-shape-rejection`.
//!
//! For the classifier that is especially costly. Claude Code reads the 429 as
//! "classifier unavailable" and fails closed on every action needing a safety
//! verdict, while reads and in-workspace edits keep working, so the session
//! looks half-broken rather than disconnected. Nor does it recover on its own:
//! in the shipped 2.1.226 client the classifier demotes to a second model once
//! and offers no further candidate after that demotion lands on the session's
//! own model. That last detail was read out of the distributed binary, so it
//! describes the version measured rather than a contract.
//!
//! Measured against a live deployment (Claude Code 2.1.226, Anthropic OAuth
//! pool), same session and account, varying only the `system` field and leaving
//! headers, `tools`, `max_tokens`, `stream` and `stop_sequences` untouched:
//!
//! | classifier request | result |
//! | --- | --- |
//! | relayed unmodified | 429, 15/15 |
//! | `"You are a helpful assistant."` prepended | 429, 10/10 |
//! | Claude Code identity prepended | 200, 1/1 |
//!
//! The 200 is a single run — enough to show the block is sufficient, not enough
//! to characterize its reliability. The two 429 rows carry the weight: a
//! neutral string does not pass, so the injected sentence has to be a specific
//! one, and a gateway that wants the classifier to work has no neutral option.
//! A live probe of a side query on the same kind of bearer agreed:
//!
//! | side query `system` | result |
//! | --- | --- |
//! | a string, or a single text block, with no marker | 429 |
//! | Claude Code identity block prepended | 200 |
//! | billing block prepended | 200 |
//!
//! Two predicates decide the repair, and neither reaches past what was
//! measured. The classifier is recognized by the opening sentence of its
//! prompt at `system[0]`, whatever headers it arrives with, so that repair
//! never depends on the client identifying itself. Any other request is
//! repaired only when the client is Claude Code — an `x-claude-code-session-id`
//! header, or what [`classify_client`] reads as Claude Code (a `claude-cli/`
//! user agent or `x-app: cli`) — and its system prompt carries no accepted
//! marker. Claude Desktop's chat surface and Cowork were both measured through
//! the same relay: each leads with the billing block across three system
//! blocks, so neither is repaired, and every request was accepted upstream.
//! Third-party clients keep byte-for-byte passthrough, so the gateway never
//! claims to be Claude Code on behalf of a client that isn't. Agent SDK
//! traffic drives the Claude Code CLI and identifies as it, so it is judged
//! like any Claude Code request: one carrying the Agent SDK identity is left
//! alone, and one whose custom system prompt carries no marker is repaired.
//!
//! [`classify_client`]: crate::client_attribution::classify_client
//! [`rate_limit_kind`]: super::rate_limit_kind

use axum::http::HeaderMap;
use serde_json::{json, Value};

use crate::{
    client_attribution::{classify_client, ClientKind},
    config::ProviderConfig,
    request::RequestBody,
};

/// The block inserted when a request omits it — the string Claude Code's
/// own main loop sends, and the one measured to flip the rejection above.
const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Opening sentence of the auto-mode classifier's system prompt.
///
/// Chosen because it is the part that held still. In the shipped Claude Code
/// 2.1.226 binary the prompt is a ~38 KB template whose rule sections are
/// interpolated from separate constants, and the two classifier stages seen on
/// the wire differ in output format (`</block>` versus `</severity>` stop
/// sequences) while opening identically. Both observations come from outside
/// this repository — the distributed client and a relay capture — so nothing
/// here can re-verify them; they are recorded as the reason for the choice, not
/// as a contract. Matching deeper structural markers would bind the relay to
/// exactly the parts that vary.
///
/// Third-party relays gating on this shape add a length floor and a marker set
/// to resist forgery — read out of one such public implementation, not from any
/// specification. This predicate does not, because it decides only whether to
/// repair the operator's own outbound request, never whether to grant a client
/// access — and a client that wanted the upstream check satisfied can already
/// put an accepted marker in its own system prompt, with or without this
/// module.
const CLASSIFIER_PROMPT_PREFIX: &str =
    "You are a security monitor for autonomous AI coding agents.";

/// Markers upstream is known to accept, so a request that already carries one
/// — Claude Code's main loop, or a classifier request that has somehow grown
/// one — is left alone rather than gaining a second.
///
/// Each was seen accepted on the wire: the identity string as the injected
/// block that flipped a rejection to `200`, the Agent SDK opening at the head
/// of accepted main-loop requests, and the billing block at the head of every
/// accepted Claude Desktop chat and Cowork request. Those are observations of a
/// live endpoint rather than a published contract, and the list is certainly
/// not exhaustive. It decides whether every marker-less Claude Code request is
/// rewritten, not only the classifier, so a first-party marker missing from it
/// gains a redundant identity block ahead of that request's system prompt —
/// shifting any cache prefix the prompt had. That costs a cache miss, never a
/// rejection, which is why the list is still not extended by guesswork.
const ACCEPTED_MARKER_PREFIXES: &[&str] = &[
    "You are Claude Code, Anthropic's official CLI for Claude",
    "You are a Claude agent, built on Anthropic's Claude Agent SDK",
    "x-anthropic-billing-header:",
];

/// Why a request is about to gain the identity block, for the debug line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Repair {
    /// The auto-mode classifier, recognized by its prompt alone.
    Classifier,
    /// Any other request from a Claude Code client, such as a mod's
    /// `$.model.complete` side query.
    ClaudeCodeRequest,
}

/// Prepend the Claude Code identity block to a request that needs it: an
/// auto-mode classifier request, or a request from a Claude Code client whose
/// system prompt carries no accepted marker. `headers` are the inbound client
/// request's, since they are what identifies the client.
///
/// A no-op for every other request, and the cheap read-only check runs first so
/// those never enter [`RequestBody::mutate`], whose `Arc::make_mut` would clone
/// the whole request tree before the closure could report "no change".
pub(super) fn restore_claude_code_identity(body: &mut RequestBody, headers: &HeaderMap) {
    let Some(repair) = needs_identity(body.json(), headers) else {
        return;
    };
    body.mutate(insert_identity);
    // The gateway does not otherwise rewrite an Anthropic request body, so
    // record the cases where it does — without it a misfire leaves no trace,
    // and an operator has no way to confirm the repair is landing. `debug`
    // rather than `warn` because it marks routine repair rather than a fault;
    // how often it fires follows the client's calling pattern, which is not
    // observable from here.
    match repair {
        Repair::Classifier => tracing::debug!(
            "restored the Claude Code identity block on an auto-mode classifier request"
        ),
        Repair::ClaudeCodeRequest => tracing::debug!(
            "restored the Claude Code identity block on a Claude Code request without a first-party marker"
        ),
    }
}

/// Which repair `request` needs, or `None` when it needs none.
///
/// The classifier rule comes first and reads the body alone: a classifier
/// request missing an accepted marker is repaired whatever headers it carries,
/// and one that already has a marker is left alone. Otherwise the request is
/// repaired only when [`is_claude_code_client`] holds and its `system` carries
/// no accepted marker — a string `system` is checked as a whole, an array
/// block by block, and an absent or `null` `system` has none. A `system` of
/// any other type, or a body that is not an object, is left for upstream to
/// reject on its own terms.
fn needs_identity(request: &Value, headers: &HeaderMap) -> Option<Repair> {
    if let Some(blocks) = classifier_blocks(request) {
        return (!blocks_carry_marker(blocks)).then_some(Repair::Classifier);
    }
    if !is_claude_code_client(headers) {
        return None;
    }
    let marked = match request.as_object()?.get("system") {
        None | Some(Value::Null) => false,
        Some(Value::String(text)) => is_accepted_marker(text.trim_start()),
        Some(Value::Array(blocks)) => blocks_carry_marker(blocks),
        Some(_) => return None,
    };
    (!marked).then_some(Repair::ClaudeCodeRequest)
}

/// Whether the inbound request comes from Claude Code: it names a Claude Code
/// session, or [`classify_client`] reads its headers as Claude Code's.
///
/// [`classify_client`]: crate::client_attribution::classify_client
fn is_claude_code_client(headers: &HeaderMap) -> bool {
    headers.contains_key("x-claude-code-session-id")
        || classify_client(headers) == ClientKind::ClaudeCode
}

/// Whether any block of `blocks` opens with an accepted marker.
///
/// Every block is scanned: an accepted marker anywhere in the array is enough
/// for upstream, so its position is not ours to assume.
fn blocks_carry_marker(blocks: &[Value]) -> bool {
    blocks
        .iter()
        .any(|block| block_text(block).is_some_and(is_accepted_marker))
}

/// Whether `text`, already trimmed at the start, opens with an accepted marker.
fn is_accepted_marker(text: &str) -> bool {
    ACCEPTED_MARKER_PREFIXES
        .iter()
        .any(|prefix| text.starts_with(prefix))
}

/// The `system` blocks of `request` when it carries the auto-mode classifier's
/// request shape, or `None` when it does not.
///
/// Returning the blocks rather than a bool is what lets [`needs_identity`] scan
/// them without extracting `system` a second time — and so without the
/// unreachable "the array stopped being an array between the two calls" arm
/// that a second extraction has to write.
///
/// Deliberately independent of [`ACCEPTED_MARKER_PREFIXES`]: that list only
/// decides whether the *identity repair* would be redundant, and a classifier
/// request that already carries an accepted marker is still the classifier
/// request as far as which model should serve it is concerned.
///
/// The trigger checks the first block alone. Every classifier request captured
/// on the wire carried the prompt at `system[0]`, so matching deeper would widen
/// what the relay acts on past anything that was measured — and the trigger is
/// client-supplied text, so the narrower it is, the fewer requests the gateway
/// touches that it was never meant to.
fn classifier_blocks(request: &Value) -> Option<&Vec<Value>> {
    let blocks = request.get("system").and_then(Value::as_array)?;
    blocks
        .first()
        .and_then(block_text)
        .is_some_and(|text| text.starts_with(CLASSIFIER_PROMPT_PREFIX))
        .then_some(blocks)
}

/// Whether `request` carries the auto-mode classifier's request shape.
pub(super) fn is_classifier_request(request: &Value) -> bool {
    classifier_blocks(request).is_some()
}

/// The upstream model `provider` pins the auto-mode classifier to, when
/// `request` is a classifier request and the provider configures one.
///
/// Pure: spend pricing reads it too, so the record of the pin
/// ([`log_pin`]) belongs to the paths that actually send.
///
/// Unlike the identity repair this is not gated on the bearer: it is an operator
/// config choice about which model answers a permission check, not a repair of a
/// shape upstream would otherwise reject.
pub(super) fn classifier_upstream_model<'a>(
    provider: &'a ProviderConfig,
    request: &Value,
) -> Option<&'a str> {
    let classifier_model = provider.classifier_model.as_deref()?;
    is_classifier_request(request).then_some(classifier_model)
}

/// Records that a request about to be sent was pinned to the classifier model.
/// Same reasoning as the identity repair's line: the gateway does not
/// otherwise pick a model the client did not ask for, so record the one case
/// where it does. `debug` because it marks configured routine behavior.
pub(super) fn log_pin(classifier_model: &str) {
    tracing::debug!(
        classifier_model,
        "pinned an auto-mode classifier request to the configured classifier model"
    );
}

/// A system block's text with leading whitespace trimmed, or `None` when the
/// block carries no string `text` (an image block, say).
fn block_text(block: &Value) -> Option<&str> {
    block
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim_start)
}

/// Insert the identity block, returning whether the request changed.
///
/// An array `system` gains it at index 0, a string `system` becomes the
/// identity block followed by a text block carrying the original string, and
/// an absent or `null` `system` becomes the identity block alone. Upholds
/// [`RequestBody::mutate`]'s contract: every path that returns `true` mutated
/// the tree, and the fallbacks — a non-object body, a `system` of any other
/// type — leave it untouched.
fn insert_identity(request: &mut Value) -> bool {
    let Some(object) = request.as_object_mut() else {
        return false;
    };
    let identity = json!({"type": "text", "text": CLAUDE_CODE_IDENTITY});
    match object.get_mut("system") {
        // Ahead of the client's first block, and ahead of any `cache_control`
        // it carries. No classifier request captured on the wire carried one,
        // so this has not been seen to shift an established cache prefix — an
        // observation, not a guarantee.
        Some(Value::Array(blocks)) => blocks.insert(0, identity),
        // A blank string carries no instructions, and upstream rejects a text
        // block with no non-whitespace text, so it is not kept as a block.
        Some(system @ Value::String(_)) => {
            let text = system.take();
            *system = if text.as_str().is_some_and(|text| text.trim().is_empty()) {
                json!([identity])
            } else {
                json!([identity, {"type": "text", "text": text}])
            };
        }
        Some(system @ Value::Null) => *system = json!([identity]),
        Some(Value::Bool(_) | Value::Number(_) | Value::Object(_)) => return false,
        None => {
            object.insert("system".to_owned(), json!([identity]));
        }
    }
    true
}

#[cfg(test)]
mod tests;
