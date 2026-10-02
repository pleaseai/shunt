# ChatGPT/Codex backend prompt-cache affinity

The ChatGPT backend derives prompt-cache affinity from the Responses `session-id` request header (`codex-rs` `core/src/client.rs`, `responses_session_id`: "ChatGPT derives cache affinity from the Responses session-id header"). The body `prompt_cache_key` must equal that header's value; the real Codex CLI sends its raw session id in both.

## What shunt sends

shunt's outbound request (chatgpt-oauth branch of `src/adapters/responses/request.rs`, WS handshake in `websocket.rs`) carries:

- `session-id` + `thread-id` headers = the effective conversation id, in order: the inbound `x-claude-code-session-id` header, then `metadata.user_id` JSON `session_id`. A metadata-only client (no session header) gets the headers derived from metadata.
- body `prompt_cache_key` derived from the same effective id (`effective_session_id` in `src/model/responses_request.rs`). A plain `user_id`, a missing JSON `session_id`, or a session string that cannot be a header value (an escaped control character) falls back to a stable Sha256-8-byte hash of the raw `user_id` — hex, header-safe, and the same value on both sides, so header and body key are always equal.
- `x-client-request-id` + `x-codex-window-id`. The window id is `{thread}:{window}` on both transports, where `{thread}` is the conversation's session id — or a delegated turn's `{session}::{agent id}` — and the window advances by one on the first post-compaction turn (the inbound one-shot `x-claude-code-context-compacted` mark, consumed once per client turn however often the turn is dispatched). The counter never expires on idleness; the map is bounded by capacity (LRU under pressure) alone.
- a delegated turn (an inbound `x-claude-code-agent-id`) swaps `thread-id` for `{session}::{agent id}`, adds `x-codex-parent-thread-id` (the session id) and `x-openai-subagent` (the agent type, `subagent` when absent), and derives `x-client-request-id` and the window id from the child thread. `session-id` and `prompt_cache_key` keep the parent's id: codex children share the root session id too. The websocket pool keys delegated turns under the child identity, so a child never rides the parent's pooled socket or the reverse.

The api-key branch sends the four affinity headers only to the stock OpenAI host (`api.openai.com`), matching codex's api-key behavior; xAI and third-party OpenAI-compatible providers keep them absent. A Codex CLI turn routed through `[server.codex_endpoint]` to an api-key upstream keeps the thread-derived headers the CLI itself sent (`thread-id`, `x-client-request-id`, `x-codex-window-id` pass through verbatim; absent ones are generated), while `session-id` stays shunt's resolved conversation id.

## Measured

2026-09-20 (gpt-5.6-luna/sol/terra via chatgpt, gpt-6-astra via codex): before the `session-id` header landed, the backend reported zero `cached_tokens` and zero `cache_write_tokens` on every turn — 20+ probes across streaming/non-streaming, 0.3K–11K prompts, effort/thinking/tools variants, byte-identical repeats. shunt-side forwarding and usage mapping were proven correct against a local mock upstream, so the header absence was the affinity break.

After the fix (gpt-5.6-luna, ~59K-token stable system prompt): the cold turn reports zero cache, the exact replay reads 58,112/59,236 (98.1%) and an appended turn keeps the same read — matching `openai/codex#44716`, where fresh sessions report zero on the first request and reuse on exact replays.

2026-09-28 A/B (gpt-6-luna, ~50K-token prompt, control `e1ef6ab` vs this series, each turn's numbers from the backend's own `usage`): the exact replay reads 49,920/50,061 (99.7%) and an appended turn keeps it (99.6%) on every scenario — HTTP with a session header, HTTP delegated turns, websocket with a header session, websocket with a metadata-only session, websocket and HTTP post-compaction turns (whose window id advances to `{thread}:1` and whose reads hold), and websocket parent/child interleaves (the child pools under its own identity; no connection carried both). Sessionless requests read 0.0% on replay, and a direct codex-canonical arm read the same 99.7%/99.6% as shunt's series on the matched scenarios. No scenario regressed against control.

Sessionless requests (no session header, no `metadata.user_id`) send no affinity headers and no key — zero affinity, where every codex session has a root identity (`session.rs:906-913`); the plain-`user_id` hash fallback is per-user, so all of one user's conversations share one cache namespace. Both affect only clients that do not send the Claude Code session header.

A client identity below a slug's `minimal_client_version` gets 400 "The '<slug>' model is not supported when using Codex with a ChatGPT account" — the same message an unentitled slug gets (gpt-6-luna needs ≥ 0.155.0; measured 2026-09-27, the same request seconds apart at identities 0.153.3 and 0.156.0). `GET /backend-api/codex/models?client_version=<v>` lists a slug only once `<v>` reaches that floor, which tells the two causes apart.

## Codex request parity

Compared against the Codex CLI request in openai/codex `e72da2b` (`codex-rs/`); the gpt-6 slugs' full canonical shape additionally carries the responses-lite body and `service_tier: "priority"` (issue #674), which shunt does not replicate, so parity below is claimed per field, not per request.

| field | shunt | codex |
|---|---|---|
| `include` | always on the chatgpt flavor for client turns; internal calls (judge, classifier, advisor consults) never request it, so the encrypted blobs cannot inflate a bounded judge reply; gated on the client's extended thinking on the openai/xai flavors | always `["reasoning.encrypted_content"]` (`client.rs:959`) |
| `tool_choice` | the client's choice, preserved: Claude Code's forced tool calls depend on it | literal `"auto"` (`client.rs:994`) |
| `parallel_tool_calls` | defaults to true when the client omits it on the chatgpt flavor, unless an Anthropic client turned the same switch off through `tool_choice.disable_parallel_tool_use`; the client's value elsewhere | always true on non-lite models (`client.rs:995`) |
| `text` | always carries `verbosity: "medium"` on the chatgpt flavor, plus the requested `json_schema` format when one is set | the model catalog's `default_verbosity` (`client.rs:961`), the format alone when configured (`client.rs:960-975`) |
| `client_metadata` | absent (only the WS turn-state echo) | always non-empty (`client.rs:988,1004`); codex's own continuation matcher excludes it (`client.rs:337-392`) |
| `x-codex-window-id` | see above; never expires on idleness, capacity-bounded | `{thread}:{window}`, window increments on compaction (`auto_compact_window.rs:77-84`) |
| subagent turns | see above | a distinct child thread id plus the same two markers (`responses_metadata.rs:394-403`); children share the root session id (`session.rs:901-913`) |

`service_tier` is sent only when configured on the route; `stream_options` is not replicated. The verbosity difference is deliberate pending an upstream configurability request; the backend's own default for an omitted `text` object is unmeasured.

Deliberate shunt mechanisms that change the request:

| mechanism | effect |
|---|---|
| handoff notes append a system block on stage-flip turns (`src/routing/handoff.rs`) | changes `instructions` on those turns; the module documents the cache-miss cost |
| progressive tool reveal rebuilds `tools`/`tool_choice` when a tool loads | moot on the ChatGPT backend, where native `tool_search` is auto-on |
| sticky-account rotation changes `chatgpt-account-id` mid-conversation (`src/accounts.rs`) | quota- and cooldown-driven; whether the backend scopes cache per account is unmeasured |

Checked and at parity: HTTP `previous_response_id` (neither client sends it), `x-client-request-id` stability on non-delegated turns, the WS continuation property-match shape, zstd `content-encoding`, `store:false`/`stream:true`, `max_output_tokens` handling, and the reasoning round-trip (an item whose `encrypted_content` is empty still round-trips its id).

## Unmeasured

- whether the backend keys cache state past `session-id` on `chatgpt-account-id`, `thread-id`, the window id, or the subagent markers
- what Claude Code sends in `metadata.user_id`, and whether the plain form is per-user or per-conversation
- live api-key `cache_read`: no API-key probe account was available
- the minimum cacheable prompt size (zero cache below ~1.5K tokens; reads begin somewhere between that and ~50K)
