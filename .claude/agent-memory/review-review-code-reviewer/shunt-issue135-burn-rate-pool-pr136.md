---
name: shunt-issue135-burn-rate-pool-pr136
description: PR #136 (issue #135) per-account threshold + burn-rate-aware pool load balancing in src/accounts.rs — architecture, the math I traced clean, and two real (non-critical) findings.
metadata:
  type: project
---

PR #136 adds per-account/per-window soft thresholds (`threshold`, `threshold_5h/7d/fable`),
`priority` (lower = preferred), `disabled`, and opt-in `[server.pool]` burn-rate-aware ordering
to `src/accounts.rs::select_order`. Core new functions: `resolved_threshold()` (precedence:
account per-window → account general → pool per-window default → pool general default → hard,
capped at hard), `assess_quota()` → `QuotaAssessment{near, over_hard, headroom}`, and
`window_headroom()` (projects time-to-threshold-exhaustion at observed burn pace minus
time-to-reset; `+inf`/`-inf` sentinels for no-pressure/already-over cases).

I traced all four of these functions by hand against the PR's own 9 new unit tests — the
arithmetic (saturating_sub throughout, `elapsed.clamp(1, window_len)` guarding div-by-zero,
`total_cmp` for the descending-headroom sort) is correct. The `rotation` vector
(`(0..accounts.len()).filter(|&i| !accounts[i].disabled)`) partitions cleanly into
`available_under` / `near_soft` / `over_hard` / `cooled` — no double-counted or dropped
accounts. Confidence this core algorithm is correct: high.

**Why:** Documents that this specific PR's core load-balancing math does not need re-review from
scratch if referenced again (e.g. a follow-up PR extends it) — only the delta needs checking.

**Two real findings from this review** (both filed as review findings, not confirmed-fixed):
1. `GET /admin/pool` (`src/admin/mod.rs::pool()`) has a **pre-existing** (not introduced by this
   PR) `if provider.auth != AuthMode::ClaudeOauth { continue; }` guard, so Codex/`chatgpt_oauth`
   accounts never appear in the admin dashboard at all. This PR's new docs
   (`site/src/content/docs/reference/configuration.md` ~line 135, and the mirrored guide file)
   claim `disabled` accounts stay "on the admin dashboard" and that `priority`/`disabled`
   "apply to Codex pools too" — true for selection logic, false for dashboard visibility. Doc
   drift introduced by this PR's new text colliding with old, untouched code.
2. If every configured account for a provider has `disabled = true`, `select_order` correctly
   returns an empty `Vec` (by design — `rotation` filters disabled out, and the sticky
   early-return path also requires `!accounts[start].disabled`). But the caller
   (`forward_claude_oauth` in `src/adapters/anthropic/mod.rs`) then falls through its `for index
   in order` loop with zero iterations, `last_response` stays `None`, and the generic error
   "all Claude OAuth accounts failed before receiving an upstream response" is returned — which
   is misleading (no account was ever attempted; they were all administratively disabled).
   Low-confidence/minor: an operator-misconfiguration edge case, not a functional bug.

**How to apply:** On a future PR touching `src/accounts.rs` pool/threshold logic, re-verify only
the delta against this trace rather than re-deriving `window_headroom`/`resolved_threshold` from
scratch. On a future PR touching `src/admin/mod.rs`'s `pool()` handler or its docs, check whether
the `ClaudeOauth`-only filter has been lifted — if so, the doc-drift finding above is resolved.
