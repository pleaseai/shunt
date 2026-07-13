# M7 — Codex Responses WebSocket v2 transport (spec)

> **⚠️ Experimental — opt-in, default off.** Gated behind a per-provider
> `websocket = true` flag on the ChatGPT/Codex backend. The HTTP Responses path
> stays the default and the only path for xAI / OpenAI-compatible providers. The
> wire protocol and the continuation optimization are validated against
> [`openai/codex`](https://github.com/openai/codex) and
> [`raine/claude-code-proxy`](https://github.com/raine/claude-code-proxy), plus one
> live probe against the ChatGPT/Codex backend (§6). Turn on only where the
> per-turn payload savings matter.

> Companion to [`m1-responses-translation.md`](m1-responses-translation.md) and
> [`m2-chatgpt-oauth.md`](m2-chatgpt-oauth.md). Adds a WebSocket transport for the
> Codex Responses API that keeps a connection warm across a conversation and reuses
> `previous_response_id` to upload only the per-turn input delta. Reuses the whole
> M1 translation core and SSE state machine unchanged; only the transport under it
> changes. Reference: `openai/codex` → `codex-rs/core/src/client.rs`,
> `codex-rs/codex-api/src/endpoint/responses_websocket.rs`,
> `codex-rs/core/src/attestation.rs`; `raine/claude-code-proxy` →
> `src/providers/codex/{websocket.rs,continuation.rs}`.

## 1. Motivation

Issue [#32](https://github.com/pleaseai/shunt/issues/32). The Codex HTTP request
path silently drops requests above a ~372k-token ceiling (see
[codex-path context accounting](../README.md)). Real Codex avoids re-uploading the
whole transcript every turn: it holds a WebSocket connection open for a
conversation and, on each subsequent turn, sends only the **new input** with a
`previous_response_id` pointing at the prior turn — the backend keeps the earlier
context server-side even under `store: false`. That trims per-turn upload and keeps
long conversations under the ceiling.

shunt is a stateless Anthropic-Messages proxy, so it cannot prewarm on the user's
keystrokes the way the Codex client does. But it **can** reproduce the two levers
that actually reduce payload: a per-session connection pool and
`previous_response_id` continuation. This milestone adds exactly those, behind a
flag, with a conservative fallback that never sends wrong context.

## 2. Scope

- A `websocket = true` flag on the `codex` provider (`config.rs`), effective only
  when the backend is ChatGPT/Codex (`Config::codex_websocket_enabled`).
- A WebSocket transport (`src/adapters/codex_ws.rs`): handshake, the
  `response.create` frame envelope, event streaming re-encoded through the existing
  [`AnthropicSseMachine`], and handshake-error re-shaping identical to the HTTP
  path.
- A per-`x-claude-code-session-id` connection pool with TTL/size eviction, a
  connection-owned reader task that keeps each pooled socket responsive to
  upstream keepalive pings, a `Pong`-verified liveness probe on reuse, and
  invalidation on any error.
- `previous_response_id` continuation (`src/adapters/codex_continuation.rs`): the
  pure decision layer that decides whether the current input is an append-only
  extension of the previous turn and, if so, computes the delta.

Out of scope: `generate: false` speculative prewarm (§7); any change to M1
translation or the SSE machine; attestation (§3).

## 3. Go/no-go — attestation is NOT required

Codex's `supports_attestation()` is true for ChatGPT auth, but the concrete
`attestation_provider` is `None` for the CLI/TUI/exec paths — only the ChatGPT
**desktop app-server** integration supplies one, and even that is best-effort.
`generate_attestation_header_for` returns `None` when the provider is absent, so no
`x-oai-attestation` header is sent and the backend accepts the request. shunt mimics
the Codex **CLI** (it reuses `~/.codex/auth.json`), which runs without attestation;
the reference proxy sends none and works. **The transport is feasible without
reproducing attestation.**

## 4. Wire protocol (faithful to `openai/codex`)

- **Endpoint.** Same path the HTTP adapter POSTs to, scheme rewritten `https→wss`
  (`http→ws`) by [`to_websocket_url`]. For the codex backend that is
  `wss://chatgpt.com/backend-api/codex/responses`.
- **Handshake headers.** The same Codex identity headers as the HTTP path (bearer,
  `chatgpt-account-id`, `originator`, `user-agent`, `version`), with `OpenAI-Beta`
  swapped for the WebSocket protocol value
  `responses_websockets=2026-02-06` ([`WEBSOCKET_BETA_PROTOCOL`]). Hop-by-hop
  headers are dropped; `into_client_request` fills the mandatory upgrade headers.
- **Request frame.** The translated Responses request JSON with
  `{"type":"response.create"}` inserted ([`response_create_frame`]). Optional
  fields: `previous_response_id` (continuation), `client_metadata`
  (carries the `x-codex-turn-state` echo).
- **Response frames.** Each backend event is a WebSocket **Text** frame whose JSON
  `type` field equals the SSE `event:` name. shunt builds
  `ResponseEvent { event: payload["type"], data: payload }` and feeds the existing
  [`AnthropicSseMachine`] directly — no re-parse, no divergence from the HTTP path.
  Upstream `Ping` frames are answered with `Pong` by the connection-owned reader
  (§5) whether or not a turn is active, so an idle pooled socket stays alive; a
  `Binary` frame is a protocol error and ends the turn.
- **Terminal events.** `response.completed | incomplete | failed | error`. Only
  `response.completed` leaves the socket healthy enough to pool
  ([`REUSABLE_TERMINAL`]); the others evict it.
- **Handshake rejection.** A refused upgrade (401/403/429) is mapped to a
  status-bearing `CodexWsError` that carries the HTTP status, `retry-after`, and
  body. Because this happens before any event streams, the adapter transparently
  falls back to the HTTP Responses path (§8), so the client sees the normal HTTP
  outcome — never a worse one for having the flag on.

## 5. Connection pool (`codex_ws.rs`)

- Process-global `HashMap<pool_key, Arc<PoolEntry>>` keyed by
  `x-claude-code-session-id`, namespaced by the authenticated inbound client when
  `[server.auth]` is configured. A std mutex guards only map lookups/inserts (never
  held across an await); a per-connection async turn lock serializes turns of one
  session. On a multi-tenant deployment, enable inbound authentication whenever
  `websocket = true`; without it, session IDs are client-provided and cannot isolate
  different callers that choose the same value.
- **Connection-owned reader (issue #93).** On connect the socket is split and a
  dedicated reader task takes sole ownership of the read half for the connection's
  whole lifetime. It answers upstream `Ping` frames with `Pong` even while the
  connection sits **idle** between turns, so the Codex backend never closes a
  pooled socket with `keepalive ping timeout`. A turn is dispatched to that reader
  over a bounded command channel (the turn lock guarantees at most one outstanding
  turn); the reader streams the turn's events, records continuation on a clean
  completion, then returns to idle keepalive duty. The reader forwards a turn's
  events over an **unbounded** channel, so downstream backpressure (a slow or
  stalled client) never blocks the read loop and therefore never starves
  control-frame handling; the buffer is bounded in practice by one turn's output,
  after which the reader is idle again.
- **Eviction.** 30-minute idle TTL (matches the reference proxy) enforced on
  insert, plus a 10 000-entry hard cap as a churn backstop. Removing an entry from
  the pool (TTL sweep, capacity eviction, or explicit invalidation) signals the
  reader to shut down and close the socket, so neither the task nor the connection
  leaks.
- **Reuse gate.** On a pooled hit the caller acquires the turn lock, then verifies
  the socket is still live: a remote close already observed by the reader is
  rejected outright, and otherwise a `Ping` is sent and a **timely `Pong`** is
  required (a half-open socket buffers the local write and would otherwise pass, so
  a successful write alone is never treated as proof of remote liveness). If the
  reader saw a close or no `Pong` returns within the probe window, the entry is
  evicted and a fresh handshake runs before the turn's frame is ever sent — a stale
  socket cannot leak partial output into a new turn.
- **Invalidation.** Any non-clean end (error/incomplete terminal, close, transport
  error, or a rejected `previous_response_id`) evicts the connection and clears its
  continuation state. A clean `response.completed` re-pools a fresh connection and
  records fresh continuation state on it.

## 6. `previous_response_id` continuation

`previous_response_id` is **connection-scoped** under `store: false`: the backend
holds the prior context per live connection, so a replayed id is valid **only on
the exact connection that produced it**. Continuation is therefore gated on
connection **reuse** — a fresh handshake always sends the full input with no
`previous_response_id`. This also nearly eliminates `previous_response_not_found`.
The continuation state lives on the `Connection` (shared with its reader task),
not globally.

**Reuse gate** (mirrors `responses_request_properties_match` in `openai/codex`,
which excludes `input`): the current request continues only when

1. its non-input fields (`model`, `instructions`, `tools`, `reasoning`, …) match a
   stable, key-sorted [`signature`] of the previous request, **and**
2. its `input` is a strict **append-only extension** of the stored transcript
   (previous input ++ previous **output items**), **and**
3. the resulting delta (the new suffix) is non-empty.

On any mismatch — a changed non-input field, an edited/compacted prefix, a shrunk
input, or an empty delta — [`decide`] returns `None` and shunt sends the **full
input**. That is exactly today's HTTP behavior: never wrong context, only a missed
optimization.

### Normalization (the crux)

shunt translates Anthropic ⇄ Responses, so the backend's assistant `output_item` is
not byte-identical to what [`translate_request`] reconstructs when Claude Code
echoes that turn back next turn. [`normalize_item`] rewrites **both** sides to a
canonical shape before comparing. It is type-aware, each rule grounded in shunt's
own reconstruction code:

| Item type | Backend `output_item.done` extras | Canonicalization | Source |
| :-- | :-- | :-- | :-- |
| all | `id`, `phase`, `status` | strip | live probe (message/reasoning/function_call), 2026-07-13 `gpt-5.6-sol` |
| `message` content part | `annotations`, `logprobs` | strip | live probe |
| `function_call` | `id`, `status`, `namespace`; `arguments` as raw model string | strip keys; **parse `arguments`** to a value | live probe (plain tool) + `openai/codex` `ResponseItem::FunctionCall` / openai-python `ResponseFunctionToolCall` (`arguments: str`); `namespace` schema-grounded (MCP not yet probed) |
| `reasoning` | `id`; `summary` parts; (`status` and plaintext `content` absent under `store:false`) | strip `id`/`status`; normalize `summary` parts; drop plaintext `content`; `encrypted_content` round-trips verbatim | live probe 2026-07-13 + `openai/codex` `ResponseItem::Reasoning` / openai-python `ResponseReasoningItem` (`content: Optional[List[reasoning_text]]`) |

For the text/message case the reconstruction is a strict **subset**, so stripping
suffices. For `function_call` the `arguments` string is *not* a subset — the backend
sends the model's raw JSON string, but shunt's SSE machine parses it into
`tool_use.input` and the next turn re-serializes that with `serde_json` (sorted
keys, no spaces). Comparing the **parsed** values instead of the strings is what
lets tool turns continue. An unparseable string is left as-is (safe fallback). Any
residual mismatch still falls through to the full-input fallback.

**Why only shunt needs this.** The `arguments` drift is specific to shunt's
Anthropic translation, not a general Responses-proxy problem. Two mature reference
proxies keep `arguments` an **opaque string end-to-end** and so never drift: LiteLLM
translates Responses ⇄ chat-completions (both string-valued — it explicitly
`str(...)`s the arguments and never parses them), and `codex` stays in pure Responses
format both directions. shunt is the outlier because Anthropic's `tool_use.input` is
a parsed **object**, forcing a parse-then-reserialize round-trip. This is also why no
reference implementation carries an equivalent normalization allowlist to borrow —
the need is unique to translating into Anthropic's shape.

The `reasoning` and `function_call` field sets were cross-checked against **three**
independent authoritative sources, all in agreement: `openai/codex` `ResponseItem`
(the type codex round-trips as input under `store:false`), openai-python's
`ResponseReasoningItem` / `ResponseFunctionToolCall`, and LiteLLM's Responses
transformation. LiteLLM also independently confirms shunt's ChatGPT/Codex request
shaping — `store:false`, `stream:true`, force-adding `reasoning.encrypted_content`
to `include`, and allowing `previous_response_id`.

The probe also found that `response.completed.output` came back **empty** — the
authoritative items are on `response.output_item.done`, so shunt captures output
items there ([`capture_continuation`]).

### turn_state

The `x-codex-turn-state` token is captured best-effort from the handshake response
header or an event body and echoed on the next request via
`client_metadata["x-codex-turn-state"]`. It is **not** required for reuse (the
reference proxy omits it and works); it is carried when available.

### Transparent retry

If the backend rejects a replayed id (`previous_response_not_found`, matched by code
or message), the reader emits a flagged `CodexWsError` rather than forwarding it as
a client event. The adapter peeks the first event, and on that flag transparently
retries once with the full input on a fresh connection — so a rejected continuation
is invisible to the client.

## 7. prewarm vs continuation — a deliberate omission

The issue frames this as "prewarm". Two separable things:

1. **Connection reuse + `previous_response_id` delta** — the payload-reduction
   lever. A stateless proxy can and should do this. **Implemented.**
2. **`generate: false` speculative prewarm** — a Codex *client-side latency* trick:
   send input early while the user is still typing so that on submit only the delta
   ships. A stateless proxy has no "user typing" phase; forcing prewarm would mean
   **two** round-trips per turn (worse). **Not implemented, by design.**

## 8. Adapter wiring (`responses.rs`)

- `forward()` branches to `forward_websocket` when
  `Config::codex_websocket_enabled` (flag && ChatGPT/Codex backend); otherwise the
  HTTP path is unchanged.
- `open_ws_turn` starts a turn with continuation allowed, peeks the first event, and
  retries with full input on `previous_response_missing` (§6). `start_ws_turn`
  applies the [`decide`] result: on a hit it replaces `input` with the delta and
  inserts `previous_response_id` (+ the turn_state echo).
- The buffered first event is replayed ahead of the channel by both the streaming
  (`stream_events_response`) and non-streaming (`json_events_response`) drivers,
  which are otherwise the WebSocket analogs of the HTTP `stream_response` /
  `json_response`. A mid-stream transport error is surfaced as an Anthropic `error`
  SSE event so the client sees a reason, not a silent truncation.
- **HTTP fallback.** A websocket failure that happens *before* streaming begins —
  connect timeout, refused/failed handshake, or a failed frame send — is caught in
  `forward()` (which retried the turn with cloned inputs) and transparently
  re-driven over the HTTP path via `forward_http`. Enabling the flag therefore can
  never do worse than plain HTTP; only a failure after the first event has streamed
  is surfaced to the client (it is then too late to fall back).

## 9. Config & validation

```toml
[providers.codex]
# … existing codex provider settings …
websocket = true   # opt-in; effective only on the ChatGPT/Codex backend
```

- `ProviderConfig.websocket: bool` (default `false`).
  `Config::codex_websocket_enabled` returns true only when the flag is set **and**
  the provider resolves to the ChatGPT/Codex backend — the flag is inert on any
  other provider, so a misconfiguration cannot route an OpenAI/xAI request over the
  Codex WebSocket.
- Documented in [`shunt.toml.example`](../shunt.toml.example) and
  [`codex-configuration.md`](codex-configuration.md).

## 10. Security

- The same bearer / account-id headers as the HTTP path, and only the ChatGPT OAuth
  credential reaches the transport (it is gated to that backend). Tokens are never
  logged.
- Continuation state is keyed by connection and never crosses sessions;
  `previous_response_id` is replayed only on its originating socket.
- Conservative fallback: any doubt about append-only equality sends the full input,
  so continuation can never leak or drop context.

## 11. Open questions / follow-ups

- **Reasoning / function_call normalization — live-probed (issue #45, 2026-07-13).**
  The text/message rules were live-probed earlier; the `reasoning` and `function_call`
  rules are now confirmed too. A probe over the WebSocket transport captured real
  `message`, `reasoning`, and `function_call` output items and diffed them against
  `normalize_item`: **no unaccounted field**. It corrected two assumptions — the backend
  omits reasoning `status` entirely and returns an empty plaintext `content` array under
  `store:false` (both already stripped, so the append-only match is unaffected). All
  three item kinds then continued from `previous_response_id` end-to-end on a warm pool
  (delta-only turns, zero `previous_response_not_found` rejects). The remaining gap is
  namespaced/MCP tool calls, which need a live MCP server to trigger; the `namespace`
  strip stays schema-grounded until then. A `shunt.codex_continuation` counter (hit vs
  full-input fallback, per provider) now surfaces any future drift.
- **Payoff measurement.** Correctness is proven; the actual per-turn byte savings on
  long real conversations should be measured before the flag is recommended broadly.
- **Multi-process pools.** The pool is process-local; a multi-replica deployment
  gets no cross-replica reuse (each replica pools its own sessions). Acceptable for
  the single-gateway norm.
