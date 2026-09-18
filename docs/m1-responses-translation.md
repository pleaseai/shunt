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

- **Function tool:** `{ type:"function", name, description, strict:false,
  parameters: normalize(input_schema) }`.
  `strict` is pinned off so the source schema's optional properties survive. A tool entry that
  omits the field is normalized toward strict mode upstream, where a closed parameter object
  (`additionalProperties:false`) behaves as if every property were required. Measured
  2026-09-08 against `gpt-5.6-sol`, 10 forced calls per case with one required and one optional
  string property: the closed object without `strict` returned the optional property (as `""`)
  10/10, and the same object with `strict:false` omitted it 10/10. A third arm sent `strict:true`
  over a nullable-required encoding and returned it as `null` 10/10 — that arm changed flag and
  schema together, so it is a separate observation, not a third controlled case. Claude Code's
  built-in `Read` ships exactly the closed shape (captured 2026-09-10: `required` holds only
  `file_path`, `additionalProperties:false`, with `offset`/`limit` integers and `pages` a
  string), and the mechanism holds on it directly: forced calls against `gpt-5.6-luna`
  arrived without `strict` carrying `offset:0`, `limit:2000` and `pages:""` 5/5, and with
  `strict:false` carrying only `file_path` 5/5 (2026-09-10). On the xAI and Grok flavors the
  field is withheld: those backends reject several standard Responses fields (`text`,
  `service_tier`, `reasoning.summary`), and their acceptance of `strict` is unverified. The
  tool specs
  replayed inside a `tool_search_output` carry the same `strict:false` — measured 2026-09-10
  against `gpt-5.6-luna`, 3 reveal turns accepted with the loaded tool called without its
  optional property. Native client-executed
  `tool_search` omits it: that tool kind rejects the field outright (`400 Unknown parameter:
  'tools[0].strict'`, measured 2026-09-10 against `gpt-5.6-luna`), and its own schema has no
  optional properties (captured 2026-09-10: `query` and `max_results` are both required), so
  the omission costs nothing either way.
- `normalize`: ensure `type:"object"`, ensure `properties:{}`, drop non-array `required`,
  default `additionalProperties:true`, and drop any `pattern` (or `patternProperties` key)
  Python's `re` cannot compile — the backend validates `parameters` against the JSON Schema
  meta-schema with `format: regex` checked by Python, and one JavaScript-only regex
  (`\p{Cc}`, `(?<name>…)`, `\u{…}`, `\z`) fails the whole request with
  `Invalid schema for function '…': '…' is not a 'regex'`. Claude Code's `Artifact` tool
  ships such a pattern. Lookaheads and everything else Python accepts are kept, the one deliberate
  exception being `\N{…}`, which names a character in Python but is a literal `N` in
  JavaScript — the engines disagree on its meaning, so it is dropped rather than
  forwarded. Outside strict
  mode a dropped `pattern` is an advisory hint lost, not a capability
  (`src/model/responses_schema.rs`).
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
| `response.created` / `response.in_progress` | response shell | `message_start` (once): `{message:{id,role,model,content:[],stop_reason:null,usage:{input_tokens,output_tokens:0}}}` + an initial `ping`. `input_tokens` seeds a local tiktoken estimate (0 unless the provider's `count_tokens = "tiktoken"`) so Claude Code's per-subagent progress indicator isn't stuck at 0; the accurate total still arrives in the terminal `message_delta`. |
| `response.output_item.added` (`type:"message"`) | new text item | open a `text` block: `content_block_start {index, content_block:{type:"text",text:""}}`. |
| `response.output_text.delta` | `delta` (string) | `content_block_delta {index, delta:{type:"text_delta", text:delta}}`. |
| `response.output_text.done` / `response.content_part.done` | — | `content_block_stop {index}`; advance index. |
| `response.output_item.added` (`type:"function_call"`) | `call_id,name` | open a `tool_use` block: `content_block_start {index, content_block:{type:"tool_use", id:call_id, name, input:{}}}`. |
| `response.function_call_arguments.delta` | `delta` (JSON fragment) | `content_block_delta {index, delta:{type:"input_json_delta", partial_json:delta}}`. |
| `response.function_call_arguments.done` / `response.output_item.done` | full `arguments` | `content_block_stop {index}`; advance index. |
| `response.reasoning_summary_text.delta` (optional) | `delta` | either a `thinking` block delta (if we surface thinking) or drop. MVP: drop. |
| `response.completed` / `response.done` / `response.incomplete` | full `response` + `usage`, `stop_reason` | `message_delta {delta:{stop_reason, stop_sequence:null}, usage:{output_tokens,...}}` then `message_stop`. `stop_reason` = `tool_use` if any function_call emitted, else `end_turn`. `response.incomplete` (a clean, if truncated, terminal — mirrors the WebSocket transport's terminal set, `m7-codex-websocket.md` §4) is treated identically to `completed`/`done`, so a stream that ends right after it does **not** trigger the Robustness fallback below. |
| `error` / `response.failed` | error object | translate to an Anthropic error and terminate the stream. The envelope's type and status follow the error `code`, not an HTTP status (§8, "In-stream backend errors"). |

Robustness: unknown event types are ignored. If the stream ends without a terminal
event (`completed` / `done` / `incomplete`), fall back to closing any open block, emit
`message_delta` (`end_turn`) + `message_stop` from accumulated text (insightflo's fallback
shape).

Non-streaming client: run the same machine but collect blocks instead of emitting; return
`transformCodexToAnthropic`-equivalent JSON: `{id,type:"message",role:"assistant",model:<original>,content,stop_reason,stop_sequence:null,usage}`.
Exception: if the machine recorded a backend error (the `error` / `response.failed` row above),
return the mapped Anthropic error envelope as a gateway error instead of the collected message
JSON (issue #113; see `m7-codex-websocket.md` §8) — `429` for an in-stream
`rate_limit_exceeded`, else `502`; either way terminal, never replayed on the next upstream.

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

**In-stream backend errors.** A backend-sent `error` / `response.failed` event (§6) arrives on
a `200 OK` stream, so there is no upstream status to preserve. `backend_error_status` picks the
row from the error `code` instead: `rate_limit_exceeded` — the Codex backend's throttle code,
which openai/codex (rust-v0.153+) classifies as its own `RateLimitExceeded` error — maps to the
`429`/`rate_limit_error` row so Claude Code's own rate-limit handling sees the right type and
status. Every other code maps to `api_error`/`502`. Both are terminal (`failure: None`): the
event follows a 2xx acceptance, and a post-acceptance failure never re-enters the ordered
failover chain (`upstreams-failover.md` §3 — the turn is not idempotent). Pool-account cooldown
is likewise driven by HTTP status only (`pool.rs` `classify_first`); an in-stream throttle does
not cool the account down.

**Exception — misalignment steer.** A `misalignment_policy_violation` may carry
`error.misalignment.steer.message` (openai/codex rust-v0.153+): a public, customer-facing
instruction on how the agent should proceed. Claude Code surfaces only the error message, so
`map_error_value` appends the steer to it (`{message}\n\n{steer}`). The sibling
`detailed_explanation` / `error_type` fields are not forwarded — upstream treats them as
sensitive and never persists them.

## 8b. Emulated `stop_sequences` (issue #605)

The Responses API has **no `stop` parameter** — Chat Completions does, Responses does not — so a
client's Anthropic `stop_sequences` cannot be forwarded. Before #605 the field was simply never
read, and every Responses upstream silently ignored it; Claude Code's auto-mode permission
classifier sends `stop_sequences: ["</block>"]` / `["</severity>"]` and the trailing text past the
stop broke its parser, costing a retry per classification.

shunt emulates them inside the translation instead. `forward` reads the field once
(`adapters/responses/mod.rs`), threads it through `TurnOptions` → `RelayOptions` →
`AnthropicSseMachine::with_stop_sequences`, and **never** adds it to the upstream body.

**Scanner** (`model/stop_sequences.rs`). Assistant text deltas only — reasoning summaries
(`response.reasoning_summary_text.delta`) and tool-call arguments
(`response.function_call_arguments.delta`) are never scanned, so a stop string inside a thinking
block or a tool argument cannot end the turn. Each text delta is appended to a holdback, which is
searched for the earliest occurrence of any stop sequence (earliest start byte wins; ties break by
the client's order). Without a match, everything but the longest holdback suffix that is a *proper*
prefix of some stop sequence is emitted immediately — so at most `max_len - 1` bytes are ever
buffered and streaming is preserved, while a stop split across two deltas is still caught. Slicing
only happens on `char` boundaries, so a multi-byte code point is never split.

**Termination.** On a match the machine emits the text before it, closes the open block, and sends
`message_delta` with `stop_reason: "stop_sequence"` and `stop_sequence: "<matched>"`, then
`message_stop`; `stopped` makes every later event — `response.completed` included — a no-op. Text
after the stop is neither streamed nor accumulated, so the non-streaming `final_json`
reconstruction is truncated at exactly the same point and reports the same `stop_reason` /
`stop_sequence`. A held-back prefix that never completes is ordinary output: `close_any` flushes it
as a normal `text_delta` before the block's `content_block_stop`, on every path that closes a text
block.

**Abort.** Every transport but one drops the upstream the moment the stop fires: `sse_parse.rs`
drops the `reqwest` byte stream with the final chunk (streaming HTTP), and `ws_stream.rs` drops
the `CodexWsEvents` receiver (both websocket paths), which makes the codex_ws reader abandon the
turn and evict the socket — correct, since a half-consumed turn must not be pooled. The exception
is non-streaming HTTP: `json_response` reads the whole body with `upstream.text()` before the
machine sees an event, so it truncates at the match like every other path but cannot cut the
upstream short. That is the pre-existing shape of the non-streaming relay, not something the stop
emulation introduced.

Each transport keys the abort on the stop sequence specifically rather than on "the machine is
stopped": on a *normally* completed turn the upstream is not mid-turn, and aborting it there would
evict a healthy pooled socket (websocket) or close a connection reqwest could otherwise return to
its idle pool (HTTP). So an ordinary terminal — `response.completed` / `response.done` /
`response.incomplete`, or a backend error event — leaves the upstream to finish; on streaming HTTP
the rest of the body goes to `spawn_terminal_drain`, a detached read bounded by
`TERMINAL_DRAIN_BUDGET`, so the connection still returns to the idle pool. The trailing bytes
translate to nothing, since `stopped` already makes every later event a no-op.

**Usage caveat.** The stop makes every later event a no-op, so the upstream's `response.completed`
usage is never applied — on the aborting transports it never arrives, and on non-streaming HTTP it
is read but discarded. `usage_value`'s existing estimate substitution applies instead, so the turn
reports the local input estimate and `output_tokens: 0`: in `message_delta.usage` when streaming,
in the final JSON's `usage` when not. A non-streaming turn emits no `message_start` and normally
skips the estimate altogether, so `forward` widens its gate to cover a request carrying
`stop_sequences` — otherwise a stopped turn would report `input_tokens: 0` for a non-empty prompt.
The estimate stays subject to the same `count_tokens = "tiktoken"` opt-in as every other path.
Tokens the upstream generated between the match and the abort are still billed upstream —
negligible for the short completions stop sequences are used for.

## 9. Test targets (M1)

- `insta` snapshots: request translation for (plain text, multi-turn, tool_use+tool_result
  round-trip, image, tool definitions + each tool_choice variant, thinking→effort).
- Streaming: feed a captured Responses SSE fixture (text deltas + a function_call) and assert
  the emitted Anthropic SSE event sequence is well-formed and incremental.
- Error mapping table.
- Live smoke (opt-in): real `OPENAI_API_KEY` end-to-end from Claude Code.
```
