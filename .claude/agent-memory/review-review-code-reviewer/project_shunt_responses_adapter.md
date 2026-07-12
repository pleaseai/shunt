---
name: project-shunt-responses-adapter
description: shunt repo Rust/axum LLM gateway — src/adapters/responses.rs conventions and how to verify Codex header claims
metadata:
  type: project
---

shunt is a Rust/axum LLM
gateway. Repo rule: read `AGENTS.md` before working in `src/` (both root and `src/CLAUDE.md` just point there).
Key AGENTS.md points relevant to adapter review: keep gateway-owned errors in Anthropic error shape, preserve
streaming semantics (don't buffer SSE unless client requested non-streaming), prefer table-driven config over
hardcoded provider logic, always add focused test coverage for protocol changes, run fmt/clippy/tests before
declaring done.

`src/adapters/responses.rs` implements the ChatGPT/Codex `/codex/responses` and OpenAI/xAI `/responses` adapter.
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
