---
name: shunt-issue48-retry-backoff-pr122
description: shunt PR #122 (issue #48) bounded upstream retry/backoff architecture — pre-stream-only invariant, per-adapter count_tokens exclusion mechanism, and a CI-signal gotcha (CONFLICTING mergeable state does not imply broken code).
metadata:
  type: project
---

PR #122 (branch `amondnet/48`, repo `pleaseai/shunt`) added `src/retry.rs`: a
bounded, idempotent upstream retry driver (`send_with_retry`) with exponential
backoff + full jitter, used from the Anthropic/Cursor/Responses single-credential
adapters. Confirmed by full read + local `cargo build`/`clippy -D warnings`/
`test --all-features --workspace` (all green on SHA d8a048f) — this is the
reference shape for retry/backoff work in this repo going forward.

Key architectural invariants worth checking again in any follow-up PR that
touches retry or the OAuth pool paths:
- Retry only ever wraps the *pre-stream* step (obtaining `reqwest::Response`
  headers/status via `.send()`); no adapter reads/streams a body before or
  during a retry. This is structural, not just documented — verify by tracing
  every `send_with_retry` call site to its surrounding `forward()`.
- OAuth account-pool paths (`forward_claude_oauth`, `forward_chatgpt_oauth`) are
  *deliberately* excluded from `send_with_retry` — they have their own
  account-rotation failover instead. Do not flag missing retry wrapping there;
  do flag it if a future PR ever routes a pooled path through
  `send_with_retry` (that would double a failure-handling mechanism).
- `count_tokens` retry-exclusion is implemented *differently per adapter kind*,
  and this is intentional, not an inconsistency: the Anthropic adapter has an
  explicit `crate::proxy::is_count_tokens(uri)` check before choosing the retry
  policy (because Anthropic-kind count_tokens passes through to a real
  upstream call); Responses/Cursor adapters need no such check because
  `proxy.rs` intercepts and locally handles count_tokens for those kinds
  before ever reaching the adapter's `forward()`.
- `RetryConfig::validate()` guards the `multiplier < 1.0` check with an
  explicit `.is_finite()` check first — necessary because `NaN < 1.0` is
  `false` in Rust, so a naive comparison alone would silently let a NaN
  multiplier through config validation.

Process gotcha (not a code issue): `gh pr view --json mergeable` reported
`CONFLICTING`/`DIRTY` for this PR, and the GitHub check-runs API showed the
"fmt · clippy · test" CI job never ran on the head SHA (only CodeQL/Socket
Security ran, the latter explicitly logging "Skipped un-mergeable pull
request"). This is because GitHub Actions' `pull_request` trigger normally
builds the ephemeral merge ref, which doesn't exist when the PR conflicts with
`main` — so **no CI signal exists is not the same as the code is broken**.
When this happens on a shunt PR, compensate by checking out the branch locally
and running the same commands CI would (`cargo fmt --all --check`, `cargo
clippy --all-targets --all-features -- -D warnings`, `cargo test
--all-features --workspace`) rather than treating the conflict itself as a
code-quality finding.
