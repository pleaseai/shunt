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
nonzero context for codex subagents. When reviewing changes here, confirm `usage_value()` never reads
`input_tokens_estimate` and `read_usage()` never writes it — that separation is what keeps the estimate from
leaking into the authoritative `message_delta`/`final_json` total. The estimate is computed once per turn in
`adapters/responses/mod.rs::forward()` (via `count_tokens::count_input_tokens`, gated on the provider's
`count_tokens = "tiktoken"` config, the default) and threaded only to the streaming constructors
(`stream_response`/`stream_events_response`); the non-streaming `json_response`/`json_events_response` paths
don't take it, since `final_json` doesn't need a placeholder. One accepted-tradeoff nit worth knowing about: the
estimate is computed unconditionally (even for non-streaming requests that then discard it) and re-parses the
request body as JSON a second time (it was already parsed once earlier in `forward()` for `client_wants_stream`/
`thinking_enabled`) — flagged as a minor perf nit in the #112 review, not blocking.
