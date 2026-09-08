# M16 — Inbound Codex translation adapters (spec)

> Companion to [`m11-inbound-codex-endpoint.md`](m11-inbound-codex-endpoint.md) (the endpoint
> these adapters serve) and [`m1-responses-translation.md`](m1-responses-translation.md) (the
> Anthropic → Responses translation they mirror). Tracks issue #477, which follows #436.
>
> **Status: translation core only.** The modules under `src/model/inbound_responses/` are
> complete and unit-tested, but nothing dispatches to them yet. #436's routed path
> (`[[server.codex_endpoint.routes]]`, merged as #478) is the dispatch point: the follow-up
> lets a route whose provider is `kind = "anthropic"` (or a Chat-Completions backend) select the
> matching pair below instead of the byte-faithful relay. Until then the inbound Codex endpoint
> behaves exactly as M11 documents.

## 0. Why

#436 routes an inbound Responses request to a third-party upstream only when that upstream
natively implements the Responses API, because the bytes are relayed verbatim. Two upstream
classes shunt already speaks are therefore unreachable from the Codex CLI:

- **Anthropic-Messages-only providers** (`kind = "anthropic"`): every third-party preset shunt
  ships (`kimi`, `zhipu`, `minimax-cn`, …) and the documented Anthropic-shaped DeepSeek / Mimo /
  OpenRouter / Vercel endpoints.
- **Chat-Completions-only providers**: self-hosted vLLM / LiteLLM / Ollama-style endpoints and
  vendors that never adopted Responses.

The adapters here translate in both directions so a route can target either class with the
credential and pool state shunt already holds for it.

## 1. Modules

| Module (`src/model/inbound_responses/`) | Direction |
| :-- | :-- |
| `messages_request` | Responses request → Anthropic Messages request |
| `messages_stream` | Anthropic Messages SSE / JSON → Responses SSE / JSON, and Anthropic error → OpenAI error |
| `chat_request` | Responses request → Chat Completions request |
| `chat_stream` | Chat Completions chunks / JSON → Responses SSE / JSON, and Chat error → OpenAI error |
| `events` | Shared `ResponsesEmitter`: the Responses envelope and SSE event grammar both stream machines drive |
| `reasoning` | `encrypted_content` codec that round-trips an Anthropic thinking signature |

Both stream machines are fed one upstream event at a time and yield the Responses frames that
event justifies (`apply(event) -> Vec<String>`), the same shape as `AnthropicSseMachine` in the
opposite direction. Nothing waits for the upstream turn to end; the only accumulation is the
output-side item list the emitter needs to populate `response.completed`.

## 2. Responses event grammar (shared emitter)

Every frame carries a monotonically increasing `sequence_number` from 0 and a `type` equal to the
SSE event name. Per response:

1. `response.created`, `response.in_progress` (status `in_progress`, empty `output`).
2. Per output item, in upstream order:
   - message: `output_item.added` (`content: []`) → `content_part.added` (`output_text`, empty) →
     `output_text.delta`… → `output_text.done` → `content_part.done` → `output_item.done`
     (`status: "completed"`, one `output_text` part).
   - reasoning: `output_item.added` (`summary: []`) → `reasoning_summary_part.added` →
     `reasoning_summary_text.delta`… → `reasoning_summary_text.done` →
     `reasoning_summary_part.done` → `output_item.done` (`summary: [{summary_text}]`, plus
     `encrypted_content` when the source carried a signature).
   - function call: `output_item.added` (`call_id`, `name`, `arguments: ""`) →
     `function_call_arguments.delta`… → `function_call_arguments.done` → `output_item.done`
     (`arguments` defaults to `"{}"` when nothing was streamed).
   - web search: `output_item.added` → `web_search_call.in_progress` / `searching` / `completed`
     → `output_item.done` (`action.type: "search"`, `action.query` when known).
3. Terminal, exactly one: `response.completed` (with `usage`), `response.incomplete`
   (`incomplete_details.reason`, with `usage`), or `response.failed` (`error.code`,
   `error.message`). Frames applied after a terminal are ignored; a second terminal emits nothing.

`usage` is `{input_tokens, input_tokens_details.cached_tokens, output_tokens,
output_tokens_details.reasoning_tokens, total_tokens}` with `total = input + output`.

Closing an item index that is not open, or is open as a different kind, is a no-op that leaves
the open item intact.

## 3. Responses → Anthropic Messages (`messages_request`)

| Responses field | Messages field | Rule |
| :-- | :-- | :-- |
| `model` | `model` | Replaced by the route's upstream model. |
| `instructions` | `system` | Plus the text of any `system` / `developer` message items in `input`, joined with `\n`. |
| `max_output_tokens` | `max_tokens` | Absent → `DEFAULT_MAX_TOKENS` (8192); Anthropic requires the field. |
| `stream`, `temperature`, `top_p` | same | Passed through (numbers only). `temperature` / `top_p` are dropped when thinking is enabled. |
| `metadata.user_id` | `metadata.user_id` | |
| `input` string | one user message | |
| `input[]` | `messages[]` | Adjacent same-role items merge into one message (Anthropic requires alternation). `message` parts: `input_text` / `output_text` → `text`; `input_image` data URL → base64 `image`, http(s) → url `image`; `input_file` data URL → base64 `document` with the URL's mime. `function_call` → `tool_use` (`arguments` parsed; unparsable → `{}`). `function_call_output` → `tool_result`. `reasoning` → decoded via `reasoning::decode_thinking` into a `thinking` / `redacted_thinking` block placed **first** in its assistant message; foreign or absent `encrypted_content`, or a turn with no other assistant content, drops the item. `web_search_call` and unknown types are dropped. Whitespace-only text is skipped. |
| `tools[]` `function` | `tools[]` | `name`, `description` (when present), `input_schema` = `parameters` or an empty object schema; `strict` dropped. |
| `tools[]` `web_search*` | `web_search_20250305` | `filters.allowed_domains` → `allowed_domains`, `user_location` passed through. Other built-ins dropped. |
| `tool_choice` | `tool_choice` | `auto` → `auto`, `none` → `none`, `required` → `any`, `{function,name}` → `{tool,name}`, `allowed_tools` by its `mode`, with `tools` filtered to the listed function names and to web search only when a `web_search*` entry is listed (an empty, missing, or non-list `tools` admits none). `parallel_tool_calls: false` adds `disable_parallel_tool_use: true` to `auto` / `any` / `tool`. Omitted without tools. |
| `reasoning.effort` | `thinking` | `low` 2048, `medium` 8192, `high` 16384, `xhigh` 32768 tokens, clamped to `max_tokens − 1024`; below 1024, or `none` / `minimal` / absent, no `thinking`. A forced `tool_choice` (`any` / `tool`) wins over thinking, which is then omitted. |
| `text.format` `json_schema` | `output_format` | `{type: "json_schema", schema}`. The dispatching caller is responsible for the structured-outputs beta header. `json_object` / `text` → nothing. |
| everything else | — | Dropped: `store`, `include`, `prompt_cache_key`, `previous_response_id`, `truncation`, `service_tier`, `text.verbosity`, `reasoning.summary`, `safety_identifier`, `user`. |

Errors: `NotAnObject`, `MissingInput` (no user or assistant message survived). Error text never quotes the request.

## 4. Anthropic Messages → Responses (`messages_stream`)

| Anthropic event | Emitter call |
| :-- | :-- |
| `message_start` | `usage.input_tokens + cache_read + cache_creation` → `input_tokens`, `cache_read_input_tokens` → `cached_tokens` (the exact inverse of `AnthropicSseMachine::read_usage`, so `cached ≤ input`). |
| `content_block_start` `text` / `thinking` / `tool_use` | `open_message` / `open_reasoning` / `open_function_call(id, name)`; the Anthropic block `index` maps to the emitter's output index. |
| `content_block_start` `redacted_thinking` | reasoning item opened and closed at once with `encode_thinking(Redacted)`. |
| `content_block_start` `server_tool_use` | query buffered from `input_json_delta`; `web_search_call` emitted at `content_block_stop`. `web_search_tool_result` is ignored. |
| `text_delta` / `thinking_delta` / `input_json_delta` | `text_delta` / `reasoning_delta` / `arguments_delta`. `signature_delta` is retained and packed with the thinking text into `encrypted_content` at close via `encode_thinking(Signed)`. |
| `message_delta` | `stop_reason` and `usage.output_tokens` recorded. |
| `message_stop` | open blocks closed, then `completed(usage)`, or `incomplete(reason, usage)` when `stop_reason` names one: `max_tokens` → `max_output_tokens`, `model_context_window_exceeded` → `model_context_window_exceeded`. |
| `error` | open blocks closed, `failed(error.type, error.message)`; terminal. |
| `ping`, others | nothing. |

`created()` runs once, ahead of the first event the machine relays rather than only on
`message_start`, so an upstream that failed before it opened the message still emits
`response.created` + `response.in_progress` before its terminal.

`finish()` handles an upstream body that ended without a terminal: open blocks are closed and
`response.failed` with code `upstream_stream_truncated` is emitted. `translate_response` builds
the same response object from a non-streaming message. `translate_error` maps
`{"type":"error","error":{type,message}}` to `{"error":{type,message,code:null,param:null}}`
with the Anthropic `type` string carried as-is; any other shape becomes `api_error` with a fixed
message rather than an echo of the upstream body.

## 5. Responses → Chat Completions (`chat_request`)

| Responses field | Chat field | Rule |
| :-- | :-- | :-- |
| `model` | `model` | Replaced by the route's upstream model. |
| `instructions` | leading `system` message | `system` / `developer` input items become `system` messages in place. |
| `input[]` | `messages[]` | `message` parts: text → `text` part, `input_image` → `image_url`, `input_file` data URL → `file`; all-text content collapses to a plain string (joined with `\n`). `function_call` → assistant `tool_calls` (consecutive calls merge; a call after assistant text merges into that message). `function_call_output` → `tool` message (`tool_call_id`, text joined with `\n`). `reasoning`, `web_search_call`, unknown types dropped. |
| `stream: true` | `stream: true` + `stream_options.include_usage: true` | So the final chunk carries `usage`. |
| `max_output_tokens` | `max_completion_tokens` | |
| `reasoning.effort` | `reasoning_effort` | String passthrough. `text.verbosity` → `verbosity`. |
| `temperature`, `top_p`, `parallel_tool_calls`, `metadata` | same | `user` / `safety_identifier` → `user`. |
| `tools[]` `function` | `{type: function, function: {name, description, parameters, strict}}` | Absent keys and explicit `null`s omitted; `parameters` absent → empty object schema. Built-ins dropped; `tools` omitted when empty. |
| `tool_choice` | `tool_choice` | `auto` / `none` / `required` passthrough; `{function,name}` → `{type: function, function: {name}}`; `allowed_tools` by `mode`, with `tools` filtered to the listed function names (an empty, missing, or non-list `tools` admits none). Omitted without tools. |
| `text.format` | `response_format` | `json_schema` (needs `name` and `schema`) → `{json_schema: {name, schema, strict?, description?}}`; `json_object` → `{type: json_object}`; `text` → nothing. |
| everything else | — | Dropped: `store`, `include`, `prompt_cache_key`, `previous_response_id`, `truncation`, `service_tier`, `reasoning.summary`, `background`. |

Errors: `NotAnObject`, `MissingInput` (no user, assistant, or tool message survived; a turn that
folds entirely into `system` messages, such as bare `instructions`, is rejected here rather than
by the backend). Error text never quotes the request.

Follow-up, not implemented: echoing a dropped `reasoning` item back as DeepSeek's
`reasoning_content` on the next assistant message.

## 6. Chat Completions → Responses (`chat_stream`)

`apply` takes one SSE `data:` payload (`[DONE]` is the end-of-stream sentinel), reads
`choices[0].delta`:

- `reasoning_content` / `reasoning` (DeepSeek, OpenRouter, vLLM spellings) → a reasoning item,
  closed when the first text or tool call arrives, without `encrypted_content`.
- `content` → a message item; text resuming after a tool call opens a new message item.
- `tool_calls[]` keyed by `index`: an entry with `id` / `function.name` opens a function call
  (closing any open message or reasoning item, but not the other calls — parallel calls stay open
  alongside each other), later entries for the same index append `function.arguments`. Open calls
  close together on the next text or reasoning delta, or at the end of the turn.
- `finish_reason` is remembered, not emitted, because the usage chunk may still follow. `length` →
  incomplete `max_output_tokens`, `content_filter` → incomplete `content_filter`.
- `usage` → `prompt_tokens`, `prompt_tokens_details.cached_tokens` (clamped to `input_tokens`, so
  a provider that over-reports cached tokens cannot break `cached ≤ input`), `completion_tokens`,
  `completion_tokens_details.reasoning_tokens`.
- `[DONE]` → open items closed, then the remembered terminal. A streamed `{"error": …}` chunk →
  `response.failed`. An unparsable chunk is never logged verbatim; it marks the stream damaged, so
  the terminal becomes `response.failed` with code `upstream_stream_malformed` however the stream
  then ends.

`finish()` emits the startup frames first when the stream failed before any of them were sent.
`translate_response` and `translate_error` mirror §4; Chat errors are already OpenAI-shaped and
pass through with `type` / `code` / `param` defaulted when missing, and `message` replaced by the
same fixed string the in-stream failure path uses when the body named none as a string.

## 7. Wiring (follow-up on #436)

The dispatch that selects these adapters is deliberately not in this change: #436's routed
path (#478) landed while this translation core was being built, and both touch the same
`codex_endpoint` files. The follow-up:

1. Relaxes #478's "route must target `kind = "responses"`" validation to also accept
   `kind = "anthropic"` (and a Chat-Completions kind or flag, to be decided there).
2. In the routed forward, matches on the provider kind: translate the request, POST to the
   provider's `/v1/messages` or `/chat/completions` with the provider's credential (adding the
   Anthropic structured-outputs beta header when `output_format` is present), and pipe the
   upstream body through the matching stream machine — or `translate_response` when the client
   did not ask for streaming.
3. Keeps gateway-owned errors in the OpenAI Responses error shape on this endpoint (M11), using
   `translate_error` for upstream-authored ones.
4. Updates the site guide and README once the behavior is reachable from config.

Model discovery for Codex (an OpenAI-shaped `GET /models` on the Codex endpoint listing the
configured routes) stays an open question from the issue; nothing here decides it.
