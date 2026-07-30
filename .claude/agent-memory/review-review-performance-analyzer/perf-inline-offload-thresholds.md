---
name: perf-inline-offload-thresholds
description: shunt's inline-vs-spawn_blocking thresholds are calibrated to Tokio's 100 µs worker budget with divan benches; compressed input size must never gate decompression work
metadata:
  type: project
---

shunt gates CPU-bound work between "inline on the async executor" and
`offload::spawn_bounded` (blocking pool + CPU-sized semaphore). The house rule for
any new threshold:

1. **Budget is Tokio's 100 µs worker-blocking budget**, not "under a millisecond".
   Documented precedents: `cursor/connect.rs::INLINE_GZIP_OUTPUT_BYTES` = 32 KiB
   (measured 54–65 µs) and `cursor/mod.rs::INLINE_IMAGE_DECODE_BYTES` = 64 KiB
   (measured 54.55 µs; 128 KiB = 112.7 µs → rejected).
2. **Every threshold is backed by a retained divan benchmark with median numbers
   in the doc comment.** A threshold asserted from vendor throughput claims is
   not accepted precedent.
3. **Compressed size cannot bound decompression work.** `connect.rs:128-130`
   spells this out: the input-size check is only a cheap early-out; the real
   safety bound is an *output*-byte probe (`decode_gzip_frame_within` reads
   `budget + 1` and returns `None` to force the offload). This was learned the
   hard way in issue #254 / PR #256. A new codec path that gates inline decode on
   compressed length alone repeats that bug.
4. Bounded-decode helpers size the output buffer from the input
   (`Vec::with_capacity(min(payload.len() * 4, budget + 1))`) rather than
   growing from `Vec::new()`.

**Why:** these thresholds exist to keep one request's CPU work from stalling every
other request's SSE relay on the same worker thread; an over-large inline budget
silently reintroduces the stall the offload machinery was added to prevent.

**How to apply:** when reviewing or writing a new inline/offload gate, check it
against the 100 µs budget, demand a bench, and for any decompression check that
the gate is keyed on decoded output bytes. Compression is ~5–10× slower per byte
than decompression, so a compression threshold cannot simply copy a decode one.
