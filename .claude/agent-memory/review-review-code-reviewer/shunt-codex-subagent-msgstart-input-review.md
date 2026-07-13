---
name: shunt-codex-subagent-msgstart-input-review
description: amondnet/codex-subagent-msgstart-input branch is stale vs origin/main (diverged before PRs #103/#104/#108) — diff against merge-base, not raw origin/main; plus the usage_value() zero-sentinel finding from iteration-2 review.
metadata:
  type: project
---

## Stale-branch diff gotcha

`amondnet/codex-subagent-msgstart-input` branched off `583d0c5` (post-#85, pre-#95
release). `origin/main` has since gained unrelated PRs (#103 docs, #104 docs, #108
codex-ws continuation+metrics refactor). A raw `git diff origin/main -- <files>`
therefore mixes this branch's real changes with the reverse-diff of those
unrelated upstream PRs (e.g. it appeared to "remove" `apply_continuation` and
`record_continuation_outcome` metrics — those are just PR #108 changes this
branch predates, not a regression introduced here).

**How to apply:** when asked to review this branch (or any branch that might be
stale), always compute `git merge-base HEAD origin/main` first and diff against
that, or use `git diff HEAD` for uncommitted-only changes. Don't trust
`git diff origin/main` at face value when the branch might not be rebased —
check `git log HEAD..origin/main` for drift before treating every line in the
diff as this branch's own doing.

## input_tokens_estimate feature (dc30a9e + uncommitted iteration-2 fix)

`src/model/responses.rs` `usage_value()` uses `self.input_tokens == 0 &&
self.cache_read_tokens == 0` as a sentinel for "real usage never observed" to
decide whether to fall back to the seeded tiktoken estimate. This conflates
"not yet observed" with "genuinely zero" — if upstream ever legitimately
reports `input_tokens: 0` (degenerate/empty turn), the estimate would wrongly
override it. Judged low-severity/low-likelihood in review (real Responses API
turns always carry non-trivial instructions), but it's a real ambiguity if a
future bug report shows an inflated context number on a truly-empty turn.

`spawn_blocking(...).await.unwrap_or(0)` in `src/adapters/responses/mod.rs`
silently drops a JoinError with no `tracing::warn!` — but this matches existing
codebase convention (`src/adapters/anthropic/mod.rs` `account_is_static_store_token`
and `src/auth/mod.rs` `account_uuid` both do the same `unwrap_or`/`.ok()` silent-fallback
for best-effort blocking tasks), so it's not a deviation worth flagging on its own.

See [[project_shunt_responses_adapter]] for the broader responses-adapter AGENTS.md
rules.
