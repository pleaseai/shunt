---
name: shunt-pr331-auto-mode-classifier-clean
description: PR #331 auto_mode_classifier comment/doc review resolved cleanly after 3 passes; verification method for gating-invariant test comments.
metadata:
  type: project
---

PR #331 (`src/adapters/anthropic/auto_mode_classifier.rs`, issue #330) went through three
comment-accuracy review rounds. By commit `74e79bb` all three prior findings were resolved with
honest-provenance rewording rather than removal:
- third-party relay comparison → attributed to "one such public implementation, not from any
  specification" (was an unverifiable claim about a spec).
- `tracing::debug!` comment → dropped the "fires once per auto-mode action" frequency claim,
  replaced with "how often it fires follows the client's calling pattern, which is not observable
  from here" (was unfalsifiable — see [[unfalsifiable-doc-claims]]).
- `tests/passthrough.rs` comment → now correctly scoped to the single-credential `forward` path
  (default `AuthMode::Passthrough` never reaches `forward_claude_oauth`), pointing to
  `tests/multi_account.rs` for the pool path.

Verification method that worked for the new `tests/multi_account.rs` pool-path tests
(`BodyLacksIdentity`, `post_classifier_request`, and the `token_env` invariant block comment):
cross-read `src/auth/mod.rs::resolve_claude_account` (confirms `token_env` branch returns
`Credential::ClaudeOauth` unconditionally, without checking the token shape) against
`src/adapters/anthropic/mod.rs::forward_claude_oauth` (confirms `bearer_is_subscription_oauth` is
gated per-candidate on the *outbound* `Authorization` header inside the account loop, not on the
resolved `Credential` variant). Both matched their comments exactly — no rot found.

Cross-surface check across 7 places describing the same "first-system-block, per-candidate
bearer-gated" fact (module doc, `docs/implementation-plan.md`, `troubleshooting.md` × 4 locales,
and `.claude/agent-memory/review-review-security-reviewer/project_pr331-*.md`) found all seven
consistent with current code as of `74e79bb`. Worth rechecking only if the trigger widens past
`system[0]` or a gate is dropped from either call site — the security-reviewer memory note already
flags that condition for re-opening.
