---
name: pr112-message-start-estimate
description: PR #112 (amondnet/codex-subagent-msgstart-input) message_start input_tokens estimate — real gap found in the truncated-stream fallback path in src/model/responses.rs
metadata:
  type: project
---

PR #112 added a local-tiktoken `input_tokens_estimate` seeded into `message_start`'s
`usage.input_tokens` for `responses`-routed (Codex/OpenAI) models, so Claude Code's
per-subagent progress tracker (which reads the first usage snapshot) doesn't show 0
context for codex subagents. The accurate total is meant to still land in the terminal
`message_delta` once `response.completed` arrives. Touches `src/adapters/responses/mod.rs`
(`forward`, threads `input_tokens_estimate` through `stream_response` /
`stream_events_response` / `forward_http` / `forward_websocket`) and
`src/model/responses.rs` (`AnthropicSseMachine::with_input_estimate`, `start()`).

**Real finding**: `AnthropicSseMachine::usage_value()` (responses.rs ~L681) always reads
`self.input_tokens` (the real accumulator, populated only by `read_usage()` inside
`complete()` on `response.completed`/`response.done`). It never falls back to
`self.input_tokens_estimate`. `finish()` (~L143) — invoked when the upstream stream ends
*without* a terminal event (`bytes.next().await` returns `None` in `stream_response`
mod.rs:246, or the WS channel closes cleanly in `stream_events_response` mod.rs:561) —
calls `stop_events("end_turn")` → `usage_value()` with `input_tokens` still at its initial
0. Result: `message_start` shows a plausible nonzero estimate, but a truncated/incomplete
stream's final `message_delta` silently reverts to `input_tokens: 0`, with zero logging
anywhere near either `None =>` arm distinguishing "genuinely zero" from "unknown, stream
truncated." This is the *same root-cause shape* as the PR #39 finding in
[[shunt-codex-ws-error-handling]] (`finish()`/`final_json()` treat "no terminal event" the
same as "turn completed cleanly") — that PR's specific WS `Message::Close` silent-drop bug
looks fixed (codex_ws.rs now handles `Message::Close` explicitly, per comments at
L646/775), but the generic `finish()`-ignores-estimate gap in responses.rs was never
addressed, and PR #112 added a new value (`input_tokens_estimate`) that makes the
resulting divergence *visible* for the first time (before #112, both message_start and the
truncated final usage were 0 — consistent, if uninformative; after #112, message_start can
show e.g. 4000 then truncated completion still shows 0).

New test `message_start_seeds_input_token_estimate` (tests/responses_translate.rs) only
covers the happy path (`response.completed` fires); does not cover the truncated/no-
terminal-event path, so this gap shipped untested.

The other two review questions for this PR turned out to be non-issues, verified by
reading the call graph, not just plausible-sounding:
- `count_input_tokens(&body)`'s "0 on unparseable JSON" fallback is unreachable at this
  call site — `body` already survived `routing::resolve()`'s `serde_json::from_slice`
  parse in proxy.rs before `forward()` runs, so re-parsing the same bytes can't fail.
- `.unwrap_or(CountTokens::Estimate)` when `state.config.provider(&route.provider)` is
  `None` is also unreachable in practice: `routing::route_for()` (routing.rs ~L96-99) only
  ever selects `AdapterKind::Responses` when `config.provider(provider)` is `Some(..)`, so
  by the time the Responses adapter's `forward()` runs the provider lookup is guaranteed
  `Some`. Mirrors the identical pre-existing pattern in `src/proxy.rs:151-155`. Still
  inconsistent with the `.expect("route provider was validated")` idiom used for the same
  invariant in `src/adapters/anthropic/mod.rs:46-47` — worth a low-confidence style note,
  not a real bug.

**How to apply**: If re-reviewing a follow-up PR touching `AnthropicSseMachine` usage
accounting, check whether `usage_value()`/`finish()` now fall back to
`input_tokens_estimate` when the real accumulator is still 0. Also check
[[shunt-codex-ws-error-handling]] for the WS transport-close side of this same class of
bug.
