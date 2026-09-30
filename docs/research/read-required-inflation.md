# Read `required`-field inflation — root-cause attribution (2026-09)

A caller running Claude Code through shunt against the codex/ChatGPT Responses backend observed Read's model-facing schema with all four fields (`file_path`, `limit`, `offset`, `pages`) required, contradicting the tool prose ("only provide if the file is too large"; "only applicable for PDF files"). The mismatch made the model emit placeholder values (`pages: ""`) that failed pre-execution validation. This file records the attribution work and the workaround verdict; the adapter-level fix it recommends is open work.

## Attribution

Chain: Claude Code (anthropic wire) → shunt (responses wire) → codex/ChatGPT backend.

| # | hypothesis | verdict | evidence |
|---|---|---|---|
| h1 | Claude Code 2.1.263 marks all Read fields required | killed (measured) | binary Read inputSchema: bare zod `s()` on `file_path` only; `.optional()` on offset/limit/pages; the strict-mode normalizer copies `required` verbatim |
| h2 | the launcher rewrites optional → required | killed (measured) | it injects `ANTHROPIC_BASE_URL` + an apiKeyHelper env only; it does not sit in the request path |
| h3 | shunt rewrites optional → required | killed (measured) | `normalize_schema` (`src/model/responses_request.rs:856`, `required` preservation at 865–867) preserves an existing `required` array and never augments it; the running binary carried that exact symbol |
| h4 | an outer caller binding is malformed | killed (measured) | no Read-matching PreToolUse hook; no enabled plugin overrides a built-in tool's input schema |
| h5 | the remote codex/ChatGPT backend inflates `required` | unverified hypothesis | the only layer left; the upstream leg was never captured |

A healthy same-version session not through shunt exposes Read with `required: ["file_path"]`, so the schema is correct at emission and the inflation happens after local translation. Most probable mechanism (hypothesis, not measured): the backend treats `additionalProperties: false` as a strict-schema signal and repairs an incomplete `required` by promoting every property to required — consistent with exactly the four Read fields being inflated while the prose stays untouched.

## Workaround verdict

- A `PreToolUse(Read)` normalizer cannot repair the class: the malformed `pages: ""` call fails before hook dispatch (measured — no PreToolUse event fires for it), and a hook fires only after the model has already produced the call from the schema it was given.
- A `SessionStart` advisory failed both fresh treatment trials (first Read call still carried all three optional fields). A hard-disable `SessionStart` guard (never call Read; state the unavailability) passed both trials as a prevention guard, at the cost of all Read capability on the route.
- `PostToolUseFailure` has no reach here: the empty-pages rejection is not an execution failure, so the event never fires.

## Durable fix (open work)

In shunt's Responses protocol boundary (`src/model/responses_request.rs`): translate optional properties into the strict backend's nullable-required representation — every property in `required`, originally-optional ones widened to accept `null` — then strip returned `null` members only for properties recorded as originally optional, before translating the function call back to anthropic `tool_use.input`. Requires a strict-backend A/B probe on the exact translated schema, recursive schema handling, and round-trip tests (omitted, explicit-default, null-capable, nested-object, array-item shapes). A blanket null deletion would corrupt tools for which `null` is a real value. The response translator is stateless today (`AnthropicSseMachine` in `src/model/responses.rs` holds no request-side schema), so the per-tool set of originally-optional fields must be propagated explicitly from the request boundary (a lookup map parameterized on the machine, or equivalent); guessing from the response shape is how the blanket-deletion bug enters.

## Deletion trigger

Delete this file when the adapter-level fix ships and its round-trip tests land.
