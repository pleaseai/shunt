---
name: shunt-verbatim-terminology-convention
description: In the shunt repo, the word "verbatim" in comments is a strict term of art meaning byte-identical/unchanged — flag any comment that uses it loosely for "relayed without account failover" or similar non-literal senses.
metadata:
  type: project
---

Across `src/` (telemetry.rs, routes.rs, auth/mod.rs, auth/codex/store.rs, adapters/anthropic/mod.rs,
model/responses_request.rs, adapters/responses/codex_continuation.rs, model/responses.rs), every
other use of "verbatim" means literal byte-for-byte passthrough with no re-serialization — e.g.
`adapters/anthropic/mod.rs:550` "The passthrough adapter forwards the client body verbatim" and
`auth/codex/store.rs`: the imported Codex auth file "is copied verbatim — no `claudeAiOauth`-style
wrapping."

**Found one violation of this convention** in PR #114 (M10 Codex/ChatGPT account pooling):
`src/adapters/responses/mod.rs` around the `FailoverAction::Relay` arm of `forward_chatgpt_oauth`
has `// A non-failover 4xx (e.g. 400) is a client error, not the account's fault: return it
verbatim rather than rotating.` — but the actual call is `mapped_upstream_error(status, upstream,
auth).await`, which re-shapes the upstream body into a translated Anthropic-style error envelope
(`build_upstream_error` extracts a `message` field and wraps it in `{"type":"error","error":
{"type":...,"message":...}}`). This directly contradicts the module doc-comment of
`tests/codex_multi_account.rs` in the same PR, which correctly states "the Responses adapter always
re-shapes an upstream failure into an Anthropic-style error envelope... unlike the Anthropic
adapter's byte-verbatim relay." Reported at ~65 confidence / minor severity (not "Critical" — the
actual behavior is correct and intentional, only the word choice is inconsistent with the rest of
the codebase and could mislead a reader skimming for byte-passthrough guarantees).

**Why:** This repo's authors are unusually disciplined about using "verbatim" as a precise term
(always = unchanged bytes), which makes it high-signal: any comment using "verbatim" loosely (e.g.
to mean "relayed to the client" or "not retried") is worth flagging even though it's a
lower-severity nit than a stale-fact comment.

**How to apply:** When reviewing shunt comments that use "verbatim," "byte-for-byte," or "unchanged,"
trace the actual data path (does it re-serialize, translate, extract a field, or re-wrap?) before
accepting the claim — even in an otherwise very well-cross-referenced, accurate PR like #114. See
also [[shunt-otel-privacy-claim-rot]] for the higher-severity sibling pattern (a claim that's not
just imprecise wording but outright unenforced).
