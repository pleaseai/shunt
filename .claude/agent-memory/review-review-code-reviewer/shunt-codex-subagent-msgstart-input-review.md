---
name: shunt-codex-subagent-msgstart-input-review
description: amondnet/codex-subagent-msgstart-input (PR #112) branch-diff gotcha, plus status of the usage_value() zero-sentinel bug (FIXED) and the unconditional-compute/double-parse nit (FIXED) as of commit d1eda88.
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

## input_tokens_estimate feature — status as of d1eda88 (2026-07-14 re-review)

`src/model/responses.rs` `usage_value()` previously used a zero-value sentinel
to decide whether to fall back to the seeded tiktoken estimate, which conflated
"not yet observed" with "genuinely zero". **FIXED**: current code (commit
`d1eda88`, "perf(codex): overlap message_start token estimate with upstream
I/O") adds an explicit `usage_observed: bool` field, set `true` only inside
`read_usage()` when a `response.completed`/`response.done` event actually
carries `input_tokens`. `usage_value()` branches on this flag, not a zero-check,
so a genuine upstream `input_tokens: 0` is still reported as 0. Covered by a new
test `truncated_stream_falls_back_to_input_token_estimate` in
`tests/responses_translate.rs` (stream cut off before `response.completed` →
terminal `message_delta` still carries the estimate, not 0). Confirmed by
re-reading the code and running `cargo test --test responses_translate --test
passthrough --test codex_websocket_fallback` (all green) on 2026-07-14.

Also **FIXED** in the same commit: the earlier "computed unconditionally even
for non-streaming requests, re-parsing the body a second time" perf nit (see
[[project_shunt_responses_adapter]]) — `forward()` now gates `estimate_input`
on `client_wants_stream && count_tokens == Tiktoken` before ever building it,
and passes the already-parsed `request_json` (as `Arc<Value>`) into the new
`count_input_tokens_value(&Value)` (extracted from `count_input_tokens(&[u8])`
in `src/count_tokens.rs`), avoiding the second JSON parse entirely. The CPU-bound
tiktoken encode itself is now kicked off via `spawn_blocking` *before* the
upstream request/ws-connect in `forward_http`/`forward_websocket`, so it
overlaps the round-trip instead of running serially in front of it.

`spawn_blocking(...).await.unwrap_or(0)` in `src/adapters/responses/mod.rs`
silently drops a JoinError with no `tracing::warn!` — but this matches existing
codebase convention (`src/adapters/anthropic/mod.rs` `account_is_static_store_token`
and `src/auth/mod.rs` `account_uuid` both do the same `unwrap_or`/`.ok()` silent-fallback
for best-effort blocking tasks), so it's not a deviation worth flagging on its own.
This same acceptance extends to the *un-awaited* handle case introduced by the
overlap refactor: on `forward_http`'s early-return-on-non-success-upstream-status
path and on `forward_websocket`'s `open_ws_turn(...).await?` early return, the
spawned blocking task is dropped without ever being awaited (it keeps running
detached, result discarded) — the ws→http case is explicitly called out in a
code comment as an accepted rare double-encode; the forward_http case isn't
explicitly commented but is the same cosmetic-estimate/best-effort category.
Worth a low-confidence mention on future review passes but not blocking.

See [[project_shunt_responses_adapter]] for the broader responses-adapter AGENTS.md
rules.
