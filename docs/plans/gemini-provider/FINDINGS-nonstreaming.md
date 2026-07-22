# Finding: Gemini non-streaming responses come back empty

Status: **resolved** — fixed on 2026-07-22.
Scope: only affects non-streaming Gemini requests (`"stream": false` / absent).
Streaming works.

## Symptom

A non-streaming request routed to a `gemini-*` model returns a well-formed
Anthropic `message` envelope but with `content: []` (empty). `stop_reason` is
present, usage is zero-ish, no text.

## Root cause — two stacked bugs

### Bug 1 — adapter parses an SSE stream as a single JSON value
`src/adapters/gemini/mod.rs`, non-streaming branch (~lines 196–213).

The upstream is **always** called on the streaming endpoint —
`:streamGenerateContent?alt=sse` (see lines 79 and 85), regardless of whether
the client asked for streaming. So `response.text()` yields an **SSE body**
(`data: {...}\n\n` frames), not a JSON array/object.

The branch then does:
```rust
if let Ok(parsed) = serde_json::from_str::<Value>(&full_text) { ... }
```
`from_str` on an SSE blob fails, the `if let Ok` swallows the error, and
`process_chunk` is never called → machine stays empty → `final_json` returns
empty content.

**Fix direction:** parse the SSE frames the same way the streaming branch does
(split on `\n`, strip `data: ` prefix, skip empty / `[DONE]`, `from_str` each
frame, feed each to `machine.process_chunk`). Factor the line-splitting out of
the streaming closure so both paths share it. Then call
`machine.final_json(<finish_reason>)`.

### Bug 2 — text parts are streamed but never accumulated into `self.content`
`src/model/gemini.rs`, `process_part`, text case (~lines 250–266).

The `tool_use` case accumulates into `self.content` when
`self.accumulate_content` is set (lines 237–244), but the **text** case only
pushes a `content_block_delta` SSE event — it never appends the text to
`self.content`. Since `final_json` (line 361) returns `self.content`, even if
Bug 1 were fixed the non-streaming reply would still contain tool_use blocks but
**no text**.

**Fix direction:** in the text branch, when `self.accumulate_content` is true,
append/merge the text into a `type: "text"` block in `self.content` (coalesce
consecutive text deltas into one block). Do the same for the thinking branch if
non-streaming consumers should see thinking blocks.

## Test to add
`tests/gemini_translate.rs`: feed a captured multi-frame SSE body (text + a
tool_use) through the non-streaming path and assert `final_json` contains both a
non-empty `text` block and the `tool_use` block, in order. Guard against
regressing to `content: []`.

## Note
Separately, `gemini-3-flash-preview` upstream quota was very tight during
testing (429s) — not a code bug, just noise when reproducing.
