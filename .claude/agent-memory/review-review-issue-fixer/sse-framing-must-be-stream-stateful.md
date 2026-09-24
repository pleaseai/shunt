---
name: sse-framing-must-be-stream-stateful
description: Classifying a bare chunk decides SSE framing on where the network split; carry a remainder and classify complete frames only
metadata:
  type: feedback
---

Any SSE classifier that takes `&[u8]` of one chunk is wrong by construction: a
chunk boundary is the upstream's choice, not a framing event. `is_ping_only`
in `src/routing/serve/bounds.rs` classified a bare chunk, so `event: pin` +
`g\n\n` (split mid-line) and `event: ping\n` + `\n` (split at the terminator)
both read as "not a ping" and refreshed the idle deadline twice for zero
content — an endless ping stream defeated `gated_idle_ms`.

**Why:** CRLF normalization (commit `88d66037`) fixed framing *within* a chunk
and looked like the whole fix. It is not: the stateless signature is the
defect, and the normalization is orthogonal.

**How to apply:** keep a carried remainder in the stream state, extract only
frames terminated by a normalized blank line, classify those, and leave the
trailing partial buffered. Hold back a trailing lone `\r` (it may be half a
CRLF) and treat a `from_utf8` failure as "no frames yet" rather than dropping
the buffer. Decide refresh from *completed* frames only — a half-arrived frame
is not evidence of content, and counting it is exactly the hole. Test by
splitting frames INSIDE a line, not at a frame boundary, so a "buffer whole
lines" fix cannot pass for the wrong reason, and pair it with a split-content
control so the fix does not simply stall everything.
