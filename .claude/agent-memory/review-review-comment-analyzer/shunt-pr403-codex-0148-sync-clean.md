---
name: shunt-pr403-codex-0148-sync-clean
description: PR #403 (Codex client sync to openai/codex 0.148.0 — routing hint header, cache_creation_input_tokens mapping) had zero comment-rot findings; documents the cross-check method that confirmed it, including live upstream source verification via raw.githubusercontent.com.
metadata:
  type: project
---

PR #403 bumped shunt's pinned Codex CLI identity (0.144.4 -> 0.148.0), added the
`x-codex-routing-hint` header (HTTP + WS handshake, ChatGPT-OAuth arm only), and mapped upstream
`usage.input_tokens_details.cache_write_tokens` to Anthropic `cache_creation_input_tokens`
(previously hardcoded 0). All five targeted claims verified accurate against the code as written:

1. `routing_hint()`'s doc-comment claim that its tier predicate mirrors
   `src/model/responses_request.rs` — both use the literal predicate `service_tier != "default"`
   (grep confirmed identical wording, not just equivalent logic).
2. `WsTurnContext.routing_hint`'s claim that a *reused* pooled connection carries the hint of the
   turn that opened the socket, not the current turn's — confirmed in
   `src/adapters/responses/codex_ws.rs::begin()`: the reuse branch (`pool_get` hit +
   `try_lock_owned` succeeds + `probe_live`) returns early and never touches the `headers` argument
   passed in for the current turn; only the fresh-handshake branch calls `Connection::open(..,
   headers, ..)`. So a stale hint on reuse is real, not a documentation error.
3. The `cache_creation_tokens` field comment's "byte-identical to before the field existed" claim
   when the field is absent — confirmed by tracing every code path: (a) `usage_observed=true` with
   `cache_write_tokens` absent -> `cache_write.unwrap_or(0)` -> `cache_creation_tokens=0`,
   `input_tokens` computed identically to the pre-diff formula; (b) `usage_observed=false` fallback
   (stream ended before `response.completed`) -> `cache_creation_tokens` never touched, stays at its
   `0` initializer. Only one emission site exists (`usage_value()`, grepped `cache_creation_input_tokens`
   across `src/`), so no other path could diverge.
4. The upstream cross-reference claims (`X_CODEX_ROUTING_HINT_HEADER`, `build_routing_hint_header`
   in `codex-rs/core/src/client.rs`; `cache_write_tokens` with `#[serde(default)]` in
   `codex-rs/codex-api/src/sse/responses.rs`) were fetched live from
   `https://raw.githubusercontent.com/openai/codex/main/<path>` via `curl` (no gh auth needed for
   public raw content) — both symbol names, the `model={model}[;tier={tier}]` format string, and the
   suppression predicate (env_key/experimental_bearer_token/auth/aws present) matched exactly.
5. All six `0.144.4` -> `0.148.0` doc mentions (docs/, site/ across 5 locales) were updated
   consistently — grepped for leftover `0.144.4`/`144.4` repo-wide, zero hits.

**Why:** This is the clean counterexample to [[unfalsifiable-doc-claims]] (global memory) — this
PR's upstream-behavior claims were *not* unfalsifiable, because openai/codex is a public repo whose
exact source is one `curl` away. Don't skip verification just because a claim is about a
third-party upstream; check if the upstream is public and fetchable before treating the claim as
unverifiable.

**How to apply:** For shunt PRs touching the Codex/Responses adapters that cite specific
upstream `codex-rs` file paths, symbol names, or field behavior (serde defaults, gating
predicates), fetch the cited file from `raw.githubusercontent.com/openai/codex/main/<path>`
directly rather than trusting the citation — it's cheap and turns a low-confidence "plausible"
finding into a definitive pass/fail. Also reuse the "grep the single emission site" technique from
[[shunt-otel-privacy-claim-rot]] for any new field whose doc-comment makes an absence-equivalence
claim ("byte-identical", "unchanged", "same as before") — trace every code path that could produce
the field's value, not just the happy path.
