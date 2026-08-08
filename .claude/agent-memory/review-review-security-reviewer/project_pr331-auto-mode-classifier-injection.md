---
name: pr331-auto-mode-classifier-injection
description: PR #331 auto_mode_classifier — gateway mutates the client system prompt on a client-forgeable predicate; injected string is a constant (no injection vector); review findings on pool-path gating and logging were fixed in-PR.
metadata:
  type: project
---

`src/adapters/anthropic/auto_mode_classifier.rs` (PR #331, issue #330) makes the gateway
prepend a fixed system block (`"You are Claude Code, Anthropic's official CLI for Claude."`)
when the client's **first** `system` block opens with
`"You are a security monitor for autonomous AI coding agents."` and no
`ACCEPTED_MARKER_PREFIXES` marker appears anywhere in the array.

Verified security posture (after the in-PR review round):
- **No prompt-injection vector.** The inserted text is a `const`; no client data flows into it.
  `needs_identity` is read-only, `insert_identity` upholds `RequestBody::mutate`'s
  return-true-iff-mutated contract, so `raw`/`json` cannot desync.
- **Logging** is one `tracing::debug!` on the mutation path — no PII, no credential egress.
  Added because the gateway otherwise rewrites an Anthropic body nowhere else, so a misfire
  would leave no trace.
- **Gating.** Both paths now gate on `bearer_is_subscription_oauth` against the **outbound**
  header. The pool path originally skipped the check on the assumption that every account there
  is subscription-OAuth; all three `resolve_claude_account` branches do return
  `Credential::ClaudeOauth`, but the `token_env` branch wraps whatever string the env var holds
  (could be `sk-ant-api…`), so the invariant was config-dependent rather than type-enforced.
  It is now enforced per candidate in `forward_claude_oauth`.
- **Residual, and the argument against over-rating it:** the predicate is client-forgeable, so a
  client reaching the Anthropic route can have the gateway attach the accepted marker to its own
  request on the operator's pooled credentials. This is *not* a new capability — the marker is a
  public constant string, and a client can put it in its own `system[0]` and be accepted with or
  without this module, so the diff does not change what an attacker can reach. **Verified against
  the code (`f073eea` pass):** nothing on the Anthropic route strips, normalizes, or validates a
  client-supplied `system` array — `headers::filtered` is header-only, and the only body mutations
  are `normalize_upstream_model_request`, `rewrite_account_uuid_request`, and this module. (The
  `system`-scrubbing code in `src/adapters/cursor/request.rs` is the Cursor route, not this one.)
  So a co-tenant already reaches upstream with any marker it likes. The trigger was
  still narrowed to the first block to keep the rewrite inside the measured shape. Re-open if the
  injected string ever becomes client-derived, if the trigger widens past `system[0]`, or if a
  gate is dropped from either call site.

Related: [[project_m4-inbound-client-auth-security]] (inbound client gate that fronts this route),
[[project_pr224-ordered-upstreams-failover-security]].
