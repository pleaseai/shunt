---
name: pr291-zstd-decompression-security
description: PR #291 zstd on the Responses path — iteration-1 decompression bomb is CLOSED by 163d50e (ratio bound, empirically verified); residual = serde_json error echoes body content into a warn! (Sentry breadcrumb egress).
metadata:
  type: project
---

PR #291 (issue #285) adds zstd compression of outbound Responses request bodies and
zstd DEcompression of inbound bodies on `[server.codex_endpoint]`
(`codex_endpoint::model_label` → `compression::decode_zstd_within`).

## Iteration 2 (commit 163d50e): the bomb is closed — verified, not assumed

`decode_zstd_within` now computes `budget = cap.min(body.len() * MAX_DECODE_RATIO)`
(ratio 64), probes inline only for input ≤ `INLINE_ZSTD_INPUT_BYTES` (4 KiB) and only
up to `INLINE_ZSTD_OUTPUT_BYTES.min(budget)` (32 KiB), else `spawn_bounded`.
Measured locally against zstd 0.13.3 with a counting global allocator:

- 64 MiB of zeros → **2066** compressed bytes → budget `min(64 MiB, 2066*64) = 132 KiB`
  → rejected. 8 MiB zeros → 274 bytes → budget 17536 → rejected.
- Worst-case inline: ≤ 32 KiB decoded + ~128 KiB libzstd decoder state.
- `Vec` doubling overshoot is negligible, not 2×: a real 64 MiB decode under a
  64 MiB budget peaked at 64.7 MiB total allocation (~1% over).
- **A frame-declared huge windowLog is NOT an amplification vector here**: peak
  allocation was a flat ~128 KiB for windowLog 10/17/20/23/27 on a tiny payload.
  (Checked because streaming zstd is often claimed to allocate the declared window.)
- Zero-length / truncated frames → `UnexpectedEof` error, not a silent empty body.
- `offload::spawn_bounded` moves the permit *into* the `spawn_blocking` closure
  (`src/offload.rs:30-38`) — cancellation-safe; compress/decode pools are separate.

Auth order still sound: `authenticate_bearer` (`codex_endpoint.rs:188`) runs before
`to_bytes` (:214) and `model_label` (:225). Body relays verbatim — the decoded copy
never leaves `model_label`; `forward_codex_inbound` gets the original `body` (:249).

## Residual (the thing to re-check on future edits)

`parse_model`'s `NotAString` branch correctly logs only `json_type_name`, but the
`Malformed` branch logs `error = %error` (`codex_endpoint.rs:350`) and
**serde_json embeds the offending value**: a top-level JSON string body yields
`invalid type: string "<entire body>", expected struct ModelView`. Verified: a
200-char string produced a 271-char message; content is `Debug`-escaped (no log
injection) but unbounded in length. `warn!` → Sentry breadcrumb
(`observability.rs:29`) and the OTel logs bridge (`telemetry.rs:296`), so client
body content can egress — see [[sentry-pii-egress]] / [[otel-pii-egress]].

Second residual: the 64× ratio bound means ~1 MiB uploaded can force a 64 MiB
decoded copy that outlives its permit for the duration of `serde_json::from_slice`.
Ceiling equals the pre-existing #260 gap, but reachable with 64× fewer uploaded
bytes. A streaming `from_reader` over the bounded decoder would need no full copy.

See also [[pr272-cursor-offload-security]] (same offload discipline) and
[[m11-inbound-codex-endpoint-security]].
