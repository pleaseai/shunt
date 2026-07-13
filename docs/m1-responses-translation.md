# M1 — Anthropic Messages ⇄ OpenAI Responses API translation (spec)

> Companion to [`implementation-plan.md`](implementation-plan.md). This is the frozen field
> map for the `responses` adapter (§6 `adapters/responses.rs`, `codex/models.rs`,
> `model/responses.rs`). It grounds the eventual M1 Codex handoff. Reference implementation:
> `insightflo/chatgpt-codex-proxy` (TypeScript) — read during planning; **shunt deliberately
> diverges on streaming**, see §1.

## 0. Two transports, one translation core

The request/response translation below is **shared**; only endpoint + auth differ.

| Provider | URL | Auth | Extra headers |
| :-- | :-- | :-- | :-- |
| `openai` | `POST {base}/responses` (`base=https://api.openai.com/v1`) | `Authorization: Bearer <OPENAI_API_KEY>` | `OpenAI-Beta: responses=experimental` |
| `codex` / `chatgpt` | `POST https://chatgpt.com/backend-api/codex/responses` | `Authorization: Bearer <chatgpt access token>` | `chatgpt-account-id: <id>`, `OpenAI-Beta: responses=experimental`, `originator: codex_cli_rs` |

Both always send `stream: true` upstream (shunt streams to Claude Code regardless of whether
Claude Code asked for streaming — see §5).

## 1. Streaming: shunt does TRUE incremental (diverges from the reference)

`insightflo` **buffers** the entire Codex response (`await createResponse`) and only then
replays a synthetic Anthropic SSE burst. That satisfies the SSE *format* but not the gateway
contract's *incremental* requirement — Claude Code would wait for the whole generation and
can time out on long turns.

**shunt requirement:** consume the upstream Responses SSE incrementally and emit Anthropic SSE
deltas as they arrive (§6). Buffer-then-replay is acceptable only as a temporary first cut
behind a clearly-logged flag, never the default.

## 2. Request: Anthropic Messages → Responses

Anthropic request (subset shunt reads) → Responses body:

| Anthropic field | Responses field | Rule |
| :-- | :-- | :-- |
| `model` | `model` | Resolved by shunt routing: use the route's `upstream_model` if set, else the incoming id. (Do **not** re-implement insightflo's env/passthrough model guessing — routing already decided the provider; see §7 for the residual model-map/effort concern.) |
| `system` (string \| block[]) | `instructions` (string) | Concatenate text blocks with `\n`; ignore non-text system blocks. |
| `messages[]` | `input[]` (item sequence) | Per-message expansion, §3. |
| `tools[]` | `tools[]` | Map each, §4. |
| `tool_choice` | `tool_choice` | Map, §4. |
| `parallel_tool_calls` | `parallel_tool_calls` | Pass through, but see §4 (mutating-tool guard is **optional**). |
| `thinking` | `reasoning.effort` | §5. Note: Claude's `budget_tokens` and Responses `effort` are different concepts — do not arithmetic-convert. |
| — | `reasoning.summary` | `"auto"`. |
| — | `text.verbosity` | `"medium"` (default; could be config later). |
| — | `store` | `false`. |
| — | `stream` | `true` (always). |
| `max_tokens` | — | Responses has no direct equivalent used here; omit. (Revisit if truncation needed.) |

## 3. Content blocks → Responses `input` items

Walk each message's content; emit an ordered item sequence. Contiguous text/image parts of one
message coalesce into a single `message` item; tool blocks flush the pending message first.

| Anthropic block | Responses input item | Notes |
| :-- | :-- | :-- |
| `text` | `{ type:"message", role, content:[{ type, text }] }` | `type` = `output_text` when role=assistant, else `input_text`. Trim empties. |
| `image` `{source:{media_type,data}}` | `{ type:"input_image", image_url:"data:<mt>;base64,<data>" }` (a message content part) | base64 data URL. |
| `tool_use` `{id,name,input}` | `{ type:"function_call", call_id:id, name, arguments: JSON.stringify(input) }` | **`call_id` = the Anthropic `tool_use.id`** — the join key. |
| `tool_result` `{tool_use_id,content,is_error}` | `{ type:"function_call_output", call_id:tool_use_id, output }` | `output` = string content, or flattened text of block content; `is_error` with no content → `"Tool execution failed"`. |

`call_id` fidelity is critical — it links `tool_use` ↔ `tool_result` across turns. Never
regenerate it.

## 4. Tools & tool_choice

- **Tool:** `{ type:"function", name, description, parameters: normalize(input_schema) }`.
  `normalize`: ensure `type:"object"`, ensure `properties:{}`, drop non-array `required`,
  default `additionalProperties:true`.
- **tool_choice map:** `auto→"auto"`, `none→"none"`, `any→"required"`,
  `tool{name}→{type:"function",name}`. If absent but tools present → `"auto"`; if no tools →
  omit.
- **Truncation hacks are NOT adopted by default.** insightflo caps messages (>50 → last 20)
  and tools (>50 → top 30 by a hardcoded Claude-Code tool-priority table). shunt should
  **forward everything** and let upstream errors surface (they're forwarded to the developer
  per the gateway contract). If a real Codex tool/'message'-count limit bites, add an
  **opt-in, logged** cap — never silent truncation.
- **Mutating-tool parallel guard is optional.** insightflo disables `parallel_tool_calls` when
  any tool name looks mutating (edit/write/delete/…). Treat as an optional safety flag, off by
  default; document if enabled.

## 5. Reasoning effort

Effort resolution order (adopt insightflo's, config-first):
1. explicit config override (a `[providers.*] effort` or per-route setting), else
2. a model→effort table for known codex models
   (`gpt-5.2-codex→high`, `…-xhigh→xhigh`, `…-spark/-low→low`, `…-medium→medium`, …), else
3. model-name suffix parse (`-xhigh|-high|-medium|-spark|-low`), else
4. `"medium"`.

If Claude sent `thinking.type:"enabled"`, prefer a higher tier (e.g. map to `high`) — but keep
it a simple mapping, not a token-budget calculation.

Streaming vs non-streaming to Claude Code is orthogonal: shunt always streams upstream and, if
Claude Code did **not** request streaming, assembles the final Anthropic response from the
stream (§6 terminal state) and returns it as a single JSON body.

## 6. Response: Responses SSE → Anthropic SSE (state machine)

Consume upstream SSE events (each `event:`/`data:` framed; `data: [DONE]` terminates). Emit
Anthropic SSE. Maintain: a running `content_block` index, per-item open/close state, and
accumulated usage.

Upstream Responses events to handle (names as emitted by the Codex backend / Responses API):

| Upstream event | Carries | Emit (Anthropic) |
| :-- | :-- | :-- |
| `response.created` / `response.in_progress` | response shell | `message_start` (once): `{message:{id,role,model,content:[],stop_reason:null,usage:{...0}}}` + an initial `ping`. |
| `response.output_item.added` (`type:"message"`) | new text item | open a `text` block: `content_block_start {index, content_block:{type:"text",text:""}}`. |
| `response.output_text.delta` | `delta` (string) | `content_block_delta {index, delta:{type:"text_delta", text:delta}}`. |
| `response.output_text.done` / `response.content_part.done` | — | `content_block_stop {index}`; advance index. |
| `response.output_item.added` (`type:"function_call"`) | `call_id,name` | open a `tool_use` block: `content_block_start {index, content_block:{type:"tool_use", id:call_id, name, input:{}}}`. |
| `response.function_call_arguments.delta` | `delta` (JSON fragment) | `content_block_delta {index, delta:{type:"input_json_delta", partial_json:delta}}`. |
| `response.function_call_arguments.done` / `response.output_item.done` | full `arguments` | `content_block_stop {index}`; advance index. |
| `response.reasoning_summary_text.delta` (optional) | `delta` | either a `thinking` block delta (if we surface thinking) or drop. MVP: drop. |
| `response.completed` / `response.done` | full `response` + `usage`, `stop_reason` | `message_delta {delta:{stop_reason, stop_sequence:null}, usage:{output_tokens,...}}` then `message_stop`. `stop_reason` = `tool_use` if any function_call emitted, else `end_turn`. |
| `error` / `response.failed` | error object | translate to an Anthropic error (§8); terminate the stream. |

Robustness: unknown event types are ignored. If the stream ends without a terminal
`completed`, fall back to closing any open block, emit `message_delta` (`end_turn`) +
`message_stop` from accumulated text (insightflo's fallback shape).

Non-streaming client: run the same machine but collect blocks instead of emitting; return
`transformCodexToAnthropic`-equivalent JSON: `{id,type:"message",role:"assistant",model:<original>,content,stop_reason,stop_sequence:null,usage}`.

## 7. Residual model-map concern

shunt routes by model id, so the provider is already chosen. What remains is: the id Claude
Code sends (e.g. `gpt-5.2-codex` via `ANTHROPIC_CUSTOM_MODEL_OPTION`, or `claude-…` if the
developer pointed an alias at this provider) may or may not be a valid **upstream** Responses
model. Handle via route config:
- `upstream_model` on the route sets the exact id sent upstream (recommended, explicit).
- `codex/models.rs` keeps the reference model→codex table + effort table for a convenience
  fallback when `upstream_model` is unset.

## 8. Error mapping (upstream → Anthropic error shape)

Upstream (OpenAI/Codex, xAI, Cursor) HTTP error → Anthropic
`{"type":"error","error":{"type":..,"message":..}}`, per the table in
`docs/gateway-protocol.md#error-envelopes`:

| Upstream status | Anthropic `error.type` | shunt status |
| :-- | :-- | :-- |
| 400 | `invalid_request_error` | 400 |
| 401 | `authentication_error` | 401 |
| 403 | `permission_error` | 403 |
| 413 | `request_too_large` | 413 |
| 429 | `rate_limit_error` | 429 |
| 501 | `not_supported` | 501 |
| 529 | `overloaded_error` | 529 |
| 500/502/503/504 | `api_error` | same status |
| anything else | `api_error` | 502 |

The standard error statuses reach the client unchanged — `529`/`503`/`500` must not collapse to
a generic `502`, or Claude Code's overload backoff-retry (which keys off `529`) never fires.
Only a status outside that set falls back to `502`. Preserve the upstream message text where
available. (Unlike the pure pass-through path, the
`responses` path necessarily re-shapes errors because the wire format differs; that is expected
and does not conflict with the "forward errors unmodified" rule, which governs the
Anthropic→Anthropic path.)

**Exception — context overflow.** Claude Code's automatic compact-and-retry matches the literal
phrase `prompt is too long` (case-insensitive) and optionally parses `N tokens > M maximum` to
size the retry, so upstream context-length errors forwarded verbatim would strand the session
until a manual `/compact`. `map_error_value` detects them (error code
`context_length_exceeded` or message heuristics) and rewrites the message to
`prompt is too long: {actual} tokens > {limit} maximum`, keeping the upstream token counts when
the message carries two (order-agnostic: the larger is the actual count), or to the bare phrase
when it carries none.

## 9. Test targets (M1)

- `insta` snapshots: request translation for (plain text, multi-turn, tool_use+tool_result
  round-trip, image, tool definitions + each tool_choice variant, thinking→effort).
- Streaming: feed a captured Responses SSE fixture (text deltas + a function_call) and assert
  the emitted Anthropic SSE event sequence is well-formed and incremental.
- Error mapping table.
- Live smoke (opt-in): real `OPENAI_API_KEY` end-to-end from Claude Code.
```
