---
name: pr291-zstd-decompression-security
description: PR #291 zstd on the Responses path — inbound label decode is a real decompression bomb (2 KB → 64 MiB, inline, no permit); outbound compress + logging verified safe.
metadata:
  type: project
---

PR #291 (issue #285) adds zstd compression of outbound Responses request bodies and
zstd DEcompression of inbound bodies on `[server.codex_endpoint]`
(`codex_endpoint::model_label` → `compression::decode_zstd_within`).

Verified safe:

- Order in `codex_endpoint::forward`: `[server.auth]` `authenticate_bearer` runs **before**
  `to_bytes` and before `model_label`, so the decode is post-auth *when auth is configured*
  (unconfigured `[server.auth]` = single-tenant, unauthenticated — pre-existing design).
- Logging: only sizes (`body_bytes`, `compressed_bytes`) plus `content_encoding` via `Debug`
  (HeaderValue Debug escapes → no log injection). No bodies, no tokens.
- Dep: `zstd 0.13.3` / `zstd-sys 2.0.16+zstd.1.5.7`, default-features off (no dict builder,
  no legacy formats). Current libzstd, no known CVE.
- Outbound `compress_request_body` operates on server-generated bodies only.

**Why:** the residual risk is entirely on the inbound decode.

**How to apply:** the load-bearing weakness is that the offload/inline decision in
`compression::decode_zstd_within` is keyed on the **compressed input** size
(`INLINE_ZSTD_BYTES` = 64 KiB), while the budget is the **decoded** cap
(`MAX_REQUEST_BODY_BYTES` = 64 MiB). Measured locally: 64 MiB of zeros → 2069 bytes of
zstd (~32,000×), i.e. a ~2 KB request takes the *inline* branch (no semaphore permit, on
the async worker) and drives a 64 MiB `read_to_end` (Vec doubling → ~128 MiB peak
capacity) plus ~100 ms of decode. `max_concurrent_requests` (1024) is the only bound.
Re-open this if the label budget, `INLINE_ZSTD_BYTES`, or the decode's admission path
changes. See also [[pr272-cursor-offload-security]] (same offload discipline) and
[[m11-inbound-codex-endpoint-security]].
