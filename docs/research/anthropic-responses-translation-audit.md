# Anthropic ↔ OpenAI translation audit (vs rosetta-llm, 2026-09-09)

Scope: how faithfully shunt translates Anthropic Messages ↔ OpenAI Responses shapes, focused on deferred tools (`defer_loading`, `tool_search`, `tool_reference`, `server_tool_use`) and tool rewriting (definitions, calls, results). All `file:line` cites are against shunt `a04115b`, except the `normalize_schema` span, which cites the current tree (`856-870`, moved after the audit). Tests cited ran green on that revision.

## Directions

| path | status | dispatch |
|---|---|---|
| anthropic-in (Claude Code) → Responses upstream | full bidirectional translation | `src/adapters/responses/mod.rs:139` calls `translate_request_value` (`src/model/responses_request.rs:123`); response side is the stream machine in `src/model/responses.rs` |
| anthropic-in → anthropic upstream | byte-for-byte passthrough | `src/adapters/anthropic/mod.rs:65`, except the deferral strip and model rewrite |
| anthropic-in → chat-completions upstream | no translator exists | no chat request translator in `src/model/` (responses, gemini, antigravity only) |
| Responses-in (codex endpoint, routed) → anthropic upstream | translated, translation-only (dispatch unwired) | `src/model/inbound_responses/messages_request.rs:60` |
| Responses-in → chat upstream | translated, translation-only (dispatch unwired) | `src/model/inbound_responses/chat_request.rs` |
| Responses-in → chatgpt pool | byte-faithful passthrough | `src/adapters/responses/inbound.rs` |

## Deferred tools

The anthropic→responses direction runs two protocols, picked per provider/model at `src/adapters/responses/mod.rs:99-101` (`native_tool_search`): the native client-executed `tool_search` protocol (#82; auto for stock OpenAI + the ChatGPT/Codex backend, explicit `tool_search` config elsewhere) and the #43 text-based progressive-reveal shim everywhere else.

### Native protocol

- Claude Code's ToolSearch tool definition becomes the Responses `tool_search` client tool: `src/model/responses_request.rs:791`.
- A deferred function is withheld from the initial `tools` array (progressive reveal): filter at `src/model/responses_request.rs:810-816`. Loaded ones ride inside `tool_search_output.tools` instead, each carrying its full schema.
- A ToolSearch `tool_use` becomes a `tool_search_call` item with `arguments` as a native JSON object, `execution: "client"`, `call_id` = the tool_use id: `src/model/responses_request.rs:504-512`.
- A `tool_result` carrying `tool_reference` blocks becomes a `tool_search_output` item whose ordered `tools` array holds loadable specs (full `input_schema` from the request's original catalog, `defer_loading: true`, `strict: false`, deduped, unknown references skipped): `src/model/responses_request.rs:521-562`.
- An upstream `tool_search_call` renders back as a `tool_use` named `ToolSearch` with the call_id as id (synthetic `toolu_ts_{index}` fallback when upstream omits it) and a guaranteed object `input`: `src/model/responses.rs:304-350`.
- Ids round-trip losslessly in both directions.

### Shim protocol (#43)

- Deferred tools are withheld from `tools` until a `tool_reference` loads them, then forwarded with their full schema: `src/model/responses_request.rs:812-815`.
- ToolSearch itself stays a plain function tool: `src/model/responses_request.rs:819`.
- `tool_reference` results render as text — `Tool '{name}' is now available.\n\nDescription: …\n\nParameters:\n{full pretty-printed schema}`, unknown references as `Loaded tool: {name}`: `src/model/responses_request.rs:640-660`. A revealed deferred tool grows the forwarded `tools` array (issue #286).

### Anthropic-skin upstreams (OpenRouter stealth etc.)

No translation: `strip_unsupported_deferral` (`src/adapters/anthropic/deferral.rs:17`) drops `defer_loading`, `default_config.defer_loading`, `configs.*.defer_loading`, and whole `tool_search_tool*` definitions when the upstream model is not an Anthropic id (`anthropic/*` or `claude*`, `src/adapters/anthropic/deferral.rs:36-39`); a dangling `tool_choice` naming a dropped tool is removed with a mirrored predicate (`:77-87`); an empty `tools` array is dropped entirely (upstream `minItems: 1`). Anthropic-model routes keep the protocol byte-for-byte. Callsites: `src/adapters/anthropic/mod.rs:72,178,763`.

### Responses-in (codex routed path)

Deferred tools are not handled. `fold_item` (`src/model/inbound_responses/messages_request.rs:216-232`) recognizes only message / function_call / function_call_output / reasoning items; `tool_search_call` and `tool_search_output` history items are silently dropped (`_ => {}`), and `tools()` at `:399-413` maps only `function` and web-search types with no `defer_loading` output or search-tool synthesis.

### Search variants

No regex/bm25 concept exists anywhere in shunt's translation; the native protocol emits one client-executed `tool_search` shape. rosetta-llm carries variants (bm25 degrades to regex across any Responses hop); shunt does not model them at all.

## Tool rewriting (anthropic ↔ responses)

Definitions:

- `input_schema` → `parameters` via `normalize_schema` (`src/model/responses_request.rs:856-870`): forces `type: object`, defaults `properties: {}`, drops a malformed `required`, defaults `additionalProperties: true`. `strict: false` is emitted for ordinary OpenAI/Codex function tools; it is withheld on the xAI/Grok flavor (acceptance unverified) and on the native `tool_search` tool (the backend rejects the field).
- `cache_control` on tool definitions is dropped in both directions.
- `web_search_20250305` becomes the Responses hosted `web_search` tool with filters (`src/model/responses_request.rs:748-774`); dropped on the xai flavor (`:827`).
- `tool_choice`: auto/none/any → auto/none/required; a named tool → `{"type":"function","name"}`, downgraded to `auto` when the named tool was withheld by progressive reveal or is ToolSearch-on-native (`:895-909`), or is web-search on xai (`:892`) — never forces a function the backend never saw.
- Reverse direction (responses-in): `parameters` → `input_schema`, `strict` dropped (`src/model/inbound_responses/messages_request.rs:466-480`); an `allowed_tools` choice narrows the tools array itself (`:418-451`); `parallel_tool_calls: false` → `disable_parallel_tool_use` on the choice object (`:519-529`).

Calls and results:

- `tool_use` → `function_call`: `arguments` is the `input` object serialized to a string (`{}` fallback), `call_id` = the tool_use id: `src/model/responses_request.rs:467-472`.
- `tool_result` → `function_call_output`: `call_id` = `tool_use_id`; string content stays a string; blocks containing image/document become Responses content-item arrays; `tool_reference` → reveal text; an `is_error` result with no text injects `Tool execution failed`: `:665-729`.
- Thinking ↔ reasoning round-trips through a base64url `{id, enc}` payload packed into the anthropic thinking `signature` (`src/model/responses_request.rs:264-291` — `encode_reasoning_signature`, the `is_reasoning_signature` accept predicate, and `decode_reasoning_signature`); signatures shunt did not produce are dropped, never forwarded. The mirror now holds on the way back too: `src/adapters/anthropic/thinking.rs` drops the blocks whose signature shunt minted, so a turn served by a Responses (or Gemini, or Cursor) upstream cannot carry an unverifiable signature into `api.anthropic.com` on the next turn — see `docs/notes/anthropic-thinking-signatures.md`.
- Reverse direction: `function_call` → `tool_use` with the arguments string parsed back to an object (`{}` on parse failure) and `call_id` preserved: `src/model/inbound_responses/messages_request.rs:350-363`.
- `parallel_tool_calls` forwards as-is (`src/model/responses_request.rs:153-155`).

Gaps on the anthropic ↔ openai surface:

- No anthropic → chat-completions translation exists at all.
- `server_tool_use` / `web_search_tool_result` history blocks in an inbound anthropic request are silently dropped on the responses path (no case in `input_items`, `src/model/responses_request.rs:313-327`). Upstream web-search responses do render as `server_tool_use` + `web_search_tool_result` on the way back (`src/model/responses.rs:635,671`).
- `cache_control` is dropped on tools in both directions; `strict` is never honored (always false outbound, dropped inbound).
- Unknown responses-in item kinds are dropped silently by design (`fold_item` `_ => {}`).
- Anthropic `document` ↔ Responses `input_file` is mapped (url / base64 split, `:408-429`); unrepresentable sources are dropped. No audio path.

Streaming: upstream responses SSE events translate to anthropic SSE one event at a time (`src/model/responses.rs`); `function_call_arguments.delta` → `input_json_delta` partial_json (`:172-173`); a `tool_search_call` becomes a single-shot content_block_start/delta/stop triple (`:324-348`). Nothing buffers the whole turn.

## Test pins

`cargo test --test responses_translate deferred -q` — 7 passed (withholding, reveal, forced-choice downgrades).
`cargo test --test responses_translate tool_search -q` — 12 passed (native mapping, `tool_search_call`/`tool_search_output` round-trips, synthetic ids, dedup).
`cargo test --test responses_translate native_ -q` — 14 passed.
`cargo test --test inbound_codex_endpoint tool -q` — 0 matched: nothing pins the responses-in deferred-tools gap.

## Comparison verdict

For the Claude Code paths shunt owns, the deferred-tools translation is the native client-executed protocol rosetta-llm refuses (it models the server-side half only, and its BYOT refusal has a chat-bound hole). shunt's native path round-trips ids and full schemas losslessly and degrades `tool_choice` safely. rosetta-llm covers the full 3×3 format matrix including chat-completions, which shunt has no translator for. shunt's drop sites are deliberate and commented; its one unpinned gap is the responses-in path above.
