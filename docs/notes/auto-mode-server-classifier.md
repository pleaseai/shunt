# Claude Code auto mode: the server-side permission classifier

External-behaviour note for Claude Code's auto mode (measured on **2.1.278**)
and `api.anthropic.com`. Auto mode does not classify a tool use locally when it
can avoid it: it asks the API to run the permission classifier server-side, on
the session's own `/v1/messages` request, at no extra charge. That makes the
classifier part of the Messages wire protocol, and therefore something a gateway
either carries or silently breaks.

Facts below are marked **[live]** when captured on the wire through a running
gateway, and **[inferred]** when derived from client behaviour rather than
observed directly.

## The request

Two fields travel together, and the pair is load-bearing:

* the `anthropic-beta` header carries the token
  `dangerous-tool-use-2026-09-03` **[live]**;
* the body carries a top-level `safeguards` array **[live]**:

```jsonc
"safeguards": [
  {"type": "dangerous_tool_use", "classifier_context": { /* object */ }}
]
```

Without the beta token the upstream answers
`400 "safeguards: Extra inputs are not permitted"` **[live]**. The reverse —
the token without the field — is accepted, the token being inert on its own.

## The response

Non-streaming, top level **[live]**:

```jsonc
"safeguard_results": [
  {
    "type": "dangerous_tool_use",
    "status": {
      "type": "available",
      "tool_uses": {
        "toolu_…": {"type": "evaluated", "outcome": "not_flagged"}
      }
    }
  }
]
```

A turn with no tool use answers `"tool_uses": {}` **[live]**. A request that
carried no `safeguards` answers `"safeguard_results": []` **[live]**.

Streaming: the same array arrives inside the `message_delta` event, at
`delta.safeguard_results`, alongside `stop_reason` **[live]**.

Two "could not evaluate" shapes exist. Per tool use — captured on a truncated
call — `{"type": "unavailable", "reason": "truncated"}` **[live]**; the reason
set observed so far is `timeout`, `error`, `truncated`. Status-level:
`"status": {"type": "unavailable", "reason": "disabled" | "error"}` **[inferred]** —
never seen on the wire; the shape and its reason set are what the shipped client
accepts, so they are recorded as the client's contract rather than as a capture.

## What the client does with each shape

| response | client behaviour |
| --- | --- |
| `safeguard_results` present, per-tool-use `evaluated` | normal auto mode |
| a per-tool-use `unavailable` entry | classifies **that one action** locally, keeps asking the server on the next request |
| no `safeguard_results` at all | assumes the gateway dropped it and falls back to its own billed classifier for the **rest of the session** |
| `400` naming the field or the beta | denies every auto-mode tool use until `/clear` |

These four rows are **[inferred]**: the first three are read from what a session
did after each response shape, and the fourth from the client's own fallback
message for that case, not from a documented contract.

## What shunt does

**First-party verdicts.** The Anthropic adapter forwards the request's
safeguard field and beta to `api.anthropic.com`. Verdicts arrived on
`claude-sonnet-4-6` and `claude-fable-5-1` **[live]**. The response synthesis
pass leaves a present `safeguard_results` unchanged.

**Response coding.** `response_coding::normalize` incrementally decodes native
Anthropic gzip, zlib-wrapped deflate, and zstd responses before the
account-refusal judge, model alias rewriting, keepalive insertion, safeguard
synthesis, metrics and spend metering, and internal routing collectors. `proxy::post` re-encodes a marked client response with the
upstream's coding and flushes after each plaintext piece. This prevents a
compressed SSE body from waiting in a plaintext frame accumulator. Internal
judge and gated calls consume the decoded body. Gated replay retains the coding
marker for client-side encoding. A committed mixed chain relays the decoded
Anthropic winner in its existing unencoded SSE response. Informational, 204,
205, and 304 responses are not decoded or marked; a coded 200 with an empty body
still fails as truncated input.

The outbound offer retains gzip, deflate, zstd, identity entries, and `*;q=0`
exclusions in client order with their parameters. With no acceptable supported
coding, shunt sends an explicit identity offer, also when the client excludes
identity: a reply in a coding shunt cannot decode would reach the plaintext
layers and the spend meter still encoded. A third-party upstream whose thinking signatures
shunt wraps gets `identity` instead
(`thinking_wrap::request_identity_encoding`). A response in an unsupported or
stacked coding stays on the passthrough path. Codec steps
run under bounded blocking-pool admission and emit decoded pieces of at most
16 KiB. The zstd decoder does not allocate a window buffer above 8 MiB, the
largest window RFC 9659 lets a "zstd" content-coding encoder require. `Decoder` uses flate2's gzip wrapper and drains pending output before
requesting more input. At a member boundary, it starts a fresh gzip decoder
when input remains or arrives later. A malformed or truncated coding body
raises a body-stream error. See
`tests/anthropic_response_coding.rs` and
`tests/anthropic_response_coding_internal.rs` for gated delivery and routing
regressions.

**Synthesis elsewhere** (`src/proxy/safeguards.rs`). A route served by a
translation adapter (Responses/Codex, Cursor, Gemini, Antigravity) builds its own
Messages-shaped response and has no `safeguard_results` to relay. Gated on the
inbound request carrying `safeguards`, and only for 2xx responses, shunt fills
the field in where responses leave the inbound Messages surface — the
`observe_response` helper in `src/proxy/failover.rs` and the committed chain in
`src/proxy/chain_stream.rs`:

* streaming — the relayed SSE is wrapped in a frame-level transformer that
  records each `content_block_start` whose `content_block.type` is `tool_use`
  and re-serializes exactly one frame, the `message_delta`. Every other frame,
  ping and comment line is forwarded byte-for-byte, nothing is buffered past a
  frame boundary, and a frame larger than 64 KiB is forwarded unparsed. An
  event whose payload arrives as several `data:` lines is joined before parsing,
  as the SSE spec requires — parsed line by line, a split `message_delta` would
  relay unrecognized and retire the classifier for the session;
* non-streaming — the body is buffered (a JSON reply is not a stream, so no
  streaming semantics are at stake) and the field inserted with the ids of the
  `content[]` blocks whose type is `tool_use`. The buffer is capped at
  `server.limits.max_request_bytes` or 1 MiB, whichever is larger: that key
  bounds what a client may *upload*, so without the floor an operator who
  lowered it to constrain uploads would silently stop every response being
  answered. A body past the resolved cap is relayed unmodified, which retires
  the classifier for that session — shunt logs a warning naming the budget when
  it happens.

The synthesized value reports every observed tool use as
`{"type": "unavailable", "reason": "error"}` under a status-level `available`.

**Why not status-level `disabled`.** Read against the table above, a
status-level `unavailable` is as bad as sending nothing: both retire the server
classifier for the whole session, and every later action in that session is
classified by the client's own billed request. A per-tool-use `unavailable`
costs one locally classified action and leaves the session eligible — the next
request asks the server again, and a turn that lands back on a first-party route
is answered properly. `error` rather than `timeout` or `truncated` because
neither of those describes what happened: the upstream did not implement the
protocol.

**Request-side strip** (`src/adapters/anthropic/safeguards.rs`). An
Anthropic-protocol host that is not `api.anthropic.com` (Kimi, OpenRouter,
DeepSeek, Z.ai, …) may reject the unknown top-level field, and a `400` is the
worst row in the table. So for any provider whose `base_url` host is not
`api.anthropic.com`, the `safeguards` field is removed from the body and every
`dangerous-tool-use-*` token is removed from the outbound `anthropic-beta`
header, other tokens intact. The two always leave together: shipping one without
the other reproduces the very `400` this avoids. A body that carries no
`safeguards` is not re-serialized, so passthrough stays byte-for-byte.

**Translation adapters need nothing.** Responses, Cursor, Gemini and Antigravity
build their upstream bodies field by field into a fresh map; no outbound request
type carries `#[serde(flatten)]` extras, so an unknown top-level field cannot
reach those upstreams in the first place.

## Opting out

`CLAUDE_CODE_AUTO_MODE_SERVER=0` on the client stops auto mode asking the
server at all **[live]**: no request carries `safeguards`, so every path above
is inert and the classification stays on the client. There is no shunt-side
setting — the decision belongs to the client that owns the feature.
