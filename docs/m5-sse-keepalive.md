# M5 — SSE keepalive pings (middlebox timeout survival)

## 0. Problem

Proxies between Claude Code and shunt can kill an SSE stream that goes quiet: Cloudflare's
proxy returns **524 after 100 seconds without a byte** (not configurable below Enterprise).
Upstream LLM streams do go quiet — long reasoning stretches without summary deltas can exceed
that. A killed stream costs the whole turn (Claude Code retries from scratch), and for a
request that *always* thinks silently past the limit, retries fail deterministically.

The Anthropic streaming protocol already has the tool for this: **`ping` events**, which
api.anthropic.com itself emits and every client ignores. shunt injects them while the
upstream is idle, so some byte always flows before any middlebox timer expires.

## 1. Configuration

```toml
[server]
sse_keepalive_seconds = 30   # default; 0 disables injection
```

- Applies to **streaming responses only** (`text/event-stream`); JSON responses untouched.
- Default **on at 30s**: protocol-compliant (Anthropic sends pings itself), invisible to
  clients, and safely under Cloudflare's 100s with margin for two lost ticks.

## 2. Behavior

A stream wrapper (`src/keepalive.rs`) sits between the outgoing byte stream and the
response `Body` in **both adapters** (Anthropic passthrough relay and the Responses SSE
machine output):

- Upstream bytes pass through unchanged, in order, with no added latency.
- When no upstream bytes have flowed for `sse_keepalive_seconds`, inject
  `event: ping\ndata: {"type": "ping"}\n\n` and reset the idle timer.
- **Inject only at an SSE event boundary** — the previously forwarded bytes end with
  `\n\n` (or `\r\n\r\n`), or nothing has been forwarded yet. A chunk that ends mid-event
  (split frame) suppresses injection until the event completes; corrupting a frame is worse
  than a middlebox timeout. Boundary state is tracked across chunk splits.
- Upstream end/error terminates the wrapped stream exactly as before; pings are never
  emitted after the final upstream bytes.

The Anthropic adapter wraps only responses whose `content-type` is `text/event-stream`
(passthrough serves JSON on the same path). The Responses adapter's translated stream is
always SSE.

## 3. Tests

Pure-logic unit tests plus `tokio::time::pause` timing tests in `src/keepalive.rs`:

- boundary tracking: `\n\n` end, `\r\n\r\n` end, boundary split across two chunks,
  mid-event chunk ⇒ not a boundary.
- idle stream at a boundary ⇒ ping after the interval, repeated pings while still idle.
- idle stream mid-event ⇒ no ping.
- active stream (chunks faster than the interval) ⇒ output identical to input, no pings.
- upstream end ⇒ stream ends without trailing pings.

Existing integration suites must pass unchanged (no idle ⇒ no pings ⇒ byte-identical
streams).

## 4. Out of scope

- First-byte latency: shunt cannot send SSE before the upstream's response headers decide
  the status code. An upstream that takes >100s to *start* responding still 524s.
- Non-SSE long-poll responses (none exist in the gateway protocol).
