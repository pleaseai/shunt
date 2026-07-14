---
name: project-shunt-responses-adapter
description: shunt repo Rust/axum LLM gateway — src/adapters/responses/mod.rs conventions and how to verify Codex header claims
metadata:
  type: project
---

shunt is a Rust/axum LLM
gateway. Repo rule: read `AGENTS.md` before working in `src/` (both root and `src/CLAUDE.md` just point there).
Key AGENTS.md points relevant to adapter review: keep gateway-owned errors in Anthropic error shape, preserve
streaming semantics (don't buffer SSE unless client requested non-streaming), prefer table-driven config over
hardcoded provider logic, always add focused test coverage for protocol changes, run fmt/clippy/tests before
declaring done.

As of PR #112 (2026-07-14) the adapter moved from the single file `src/adapters/responses.rs` to the module
directory `src/adapters/responses/mod.rs` (plus `codex_ws.rs`, `codex_continuation.rs` submodules) — cite the
`mod.rs` path in reviews, not the old flat path. It and `src/model/responses.rs` (the `AnthropicSseMachine`
SSE state machine) were both already over the repo's 500-line-file guideline before #112; small (~20-30 line)
additive diffs to either do not meaningfully worsen that and should not be flagged as new violations.

PR #17 (branch `amondnet/refactor-adapters-split-src-adapters-responses.r`, reviewed 2026-07-14) then split the
now-1961-line `responses/mod.rs` further, into `mod.rs` (222 LOC glue/re-exports) + `error.rs` (mapped_upstream_error/
build_upstream_error/own_error) + `http.rs` (http_send/forward_http/stream_response/json_response/SseParser) +
`pool.rs` (forward_chatgpt_oauth/relay_success/with_account_header) + `request.rs` (request_builder/responses_url/
CODEX_USER_AGENT/CODEX_CLIENT_VERSION) + `websocket.rs` (forward_websocket/open_ws_turn/start_ws_turn/
apply_continuation/websocket_headers) + `ws_stream.rs` (stream_events_response/json_events_response/ws_error_sse).
Verified this was a byte-for-byte pure move (diffed every relocated function body against `git show
main:src/adapters/responses/mod.rs` line-by-line, plus targeted `diff` on the trickiest branchy functions —
`forward_chatgpt_oauth`, `apply_continuation`, `build_upstream_error`, `start_ws_turn`, `open_ws_turn`): the only
deltas anywhere were `pub(super)` visibility additions (needed since call sites now live in sibling files) and doc
comment path updates in `docs/*.md`. All 21 unit tests from the old `mod.rs::tests` module were accounted for,
redistributed to `error.rs`/`request.rs`/`websocket.rs` verbatim, none dropped. No findings reported. If asked to
review this file split again, the per-file responsibility boundaries above are now the map to navigate by.

`src/adapters/responses/mod.rs` implements the ChatGPT/Codex `/codex/responses` and OpenAI/xAI `/responses` adapter.
It gates ChatGPT-only headers (originator/user-agent/version, and session/identity headers like `session_id`,
`x-client-request-id`, `x-codex-window-id`, `accept: text/event-stream`) on the `Credential::ChatGptOAuth` match
arm inside `request_builder`. This credential variant is asserted (not just assumed) to correspond to
`config.is_chatgpt_backend()` — confirmed by checking `src/auth/mod.rs` (ChatGptOAuth is only constructed for
that backend) — so gating on the credential enum is equivalent to gating on the backend flag.

When reviewing claims in code comments here about "what the real Codex CLI sends" (e.g. header names/values,
`x-codex-window-id` format `{id}:0`, reusing the session/conversation id for both `session_id` and
`x-client-request-id`), verify via `mcp__plugin_context_grep__searchGitHub` against openai/codex (codex-rs) and
known third-party Codex proxies (icebear0828/codex-proxy, tailcallhq/forgecode, Wei-Shaw/sub2api) rather than
trusting the comment at face value — in the one case checked (2026-07-11), the claims held up (multiple
independent proxy implementations mirror the same header shape).

`src/model/responses_request.rs` (translate_request / tools() / tool_choice()) already contains multiple
pre-existing `match flavor { ResponsesFlavor::Xai => ..., _ => ... }` / `if flavor != ResponsesFlavor::Xai`
branches (service_tier, reasoning.effort, OpenAI-Beta header gating in adapters/responses.rs). This is the
established convention for flavor-specific quirks in this codebase, not a violation of the AGENTS.md "prefer
table-driven config over hardcoded provider logic" rule — that rule is aimed at avoiding stringly-typed
provider-name checks (`route.provider == "xai"`), not at the typed `ResponsesFlavor` enum match arms. Do not
flag new `match flavor` arms added to this file as a table-driven-config violation (confirmed 2026-07-12 while
reviewing the hosted-web-search-tool PR).

`AnthropicSseMachine` (in `src/model/responses.rs`) keeps prompt-size usage in two deliberately separate
fields: `input_tokens`/`cache_read_tokens` (real, set only by `read_usage()` on `response.completed`, read only
by `usage_value()` which feeds both the terminal `message_delta` and the non-streaming `final_json`) vs.
`input_tokens_estimate` (set once at construction via `with_input_estimate()`, read only by `start()` when
emitting `message_start`). Since Responses only reports real usage at completion, `message_start` would
otherwise always carry `input_tokens: 0`; PR #112 seeds it with a local tiktoken estimate so Claude Code's
per-subagent progress tracker (which reads only that first snapshot, not the merged completion usage) shows
nonzero context for codex subagents. `usage_value()` also holds an explicit `usage_observed: bool` flag (set
by `read_usage()` only when `response.completed`/`response.done` actually carries `input_tokens`) so a
truncated stream's terminal `message_delta` falls back to the estimate instead of reverting to a bare `0`, while
a genuine upstream `input_tokens: 0` still reports as 0 — this was a real bug found and fixed within PR #112
itself (see [[shunt-codex-subagent-msgstart-input-review]]), not something to re-flag unless it regresses.
When reviewing changes here, confirm `read_usage()` still sets `usage_observed = true` in the same branch that
writes `input_tokens`/`cache_read_tokens` — that pairing is what keeps the two in sync.

The estimate is computed once per turn in `adapters/responses/mod.rs::forward()` (via
`count_tokens::count_input_tokens_value(&Value)`, gated on `client_wants_stream && count_tokens == "tiktoken"`,
the provider default) from the already-parsed `request_json` (reused as `Arc<Value>`, no second JSON parse),
and threaded only to the streaming constructors (`stream_response`/`stream_events_response`); the non-streaming
`json_response`/`json_events_response` paths don't take it, since `final_json` doesn't need a placeholder. As
of commit `d1eda88` ("perf(codex): overlap message_start token estimate with upstream I/O") the CPU-bound
tiktoken encode is kicked off via `spawn_blocking` *before* `forward_http`'s upstream request / `forward_websocket`'s
websocket connect, so it overlaps that round-trip; its `JoinHandle` is only awaited once the response stream
begins. **Previously flagged, now fixed**: an earlier revision computed the estimate unconditionally (even for
non-streaming requests that then discarded it) and re-parsed the request body as JSON a second time — both
resolved by the `client_wants_stream` gate and the `count_input_tokens_value(&Value)` extraction, respectively.
One residual low-severity nit: on `forward_websocket`'s `open_ws_turn(...).await?` early-return (documented in a
code comment as an accepted rare ws→http double-encode) and on `forward_http`'s early-return-on-non-success-
upstream-status path (not commented), the spawned blocking task's `JoinHandle` is dropped without ever being
awaited — the task keeps running detached and its result is discarded. Same best-effort/cosmetic-estimate
category as the already-accepted `spawn_blocking(...).await.unwrap_or(0)` JoinError-drop pattern elsewhere in
this file; not blocking, but worth a low-confidence mention on future passes rather than omitting entirely.
