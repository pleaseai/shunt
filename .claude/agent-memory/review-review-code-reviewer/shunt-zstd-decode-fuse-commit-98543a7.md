---
name: shunt-zstd-decode-fuse-commit-98543a7
description: Codex zstd inbound decode (#291/#285) round-2 fix — decode_zstd_and_parse fusion, log-egress fix, test-module split. Verified sound.
metadata:
  type: project
---

Commit 98543a7 (branch amondnet/perf-codex-adopt-zstd-request-compression-on-the) is
the second review-fix round on issue #285's zstd inbound decode. Verified adversarially,
all six checks passed with no functional defects:

- `decode_zstd_and_parse<T, F>` in `src/compression.rs` fuses zstd decode + a caller
  `extract` closure into one bounded unit so the decoded `Bytes` (up to
  `MAX_DECODE_RATIO`=64x the compressed size) and the parse over it never cross back
  to the async executor unbounded. `budget = cap.min(body.len() * MAX_DECODE_RATIO)`;
  inline probe caps `extract`'s input at `INLINE_ZSTD_OUTPUT_BYTES.min(budget)` (32 KiB);
  inline `Ok(None)` falls through (no early return) to the offloaded re-decode, which is
  the sole authoritative over-budget answer; malformed zstd surfaces as `Err` via `?`
  inside `decode_within`, never conflated with `Ok(None)`.
- `spawn_bounded` (`src/offload.rs`) moves the permit into the `spawn_blocking` closure
  before calling `task()`, so `extract` runs while the permit is held; a panic in
  `extract` surfaces as a `JoinError`-derived `io::Error` (Tokio catches per-task panics,
  doesn't abort the process) and the permit drops normally on unwind (not poisoned,
  unlike a `Mutex` guard).
- Log-egress fix: the `Malformed` arm in `src/codex_endpoint.rs` now logs only
  `error.line()`/`error.column()`/`error.classify()` instead of `error = %error`
  (`serde_json::Error`'s Display embeds the offending value — a body that's a bare
  JSON string gets echoed in full). The zstd decode `Err` arm's `error = %error` is
  fine — libzstd-authored constant strings, not client body content.
- Test-module split (`compression.rs`→`compression/tests.rs`,
  `codex_endpoint.rs`→`codex_endpoint/tests.rs`) is a byte-for-byte pure move: ran the
  full workspace suite and reproduced exactly 1353 passed / 0 failed / 2 ignored,
  matching the commit message's claim. `#[ignore]` on `measure_inline_zstd_budgets`
  and all `#[tokio::test]` attrs survived. The bomb regression test
  (`rejects_a_decompression_bomb_via_the_ratio_bound`) still asserts its fixture
  actually exceeds `MAX_DECODE_RATIO` before testing — cannot go vacuous.

One soft (not-flagged-as-bug) observation: the inline decode branch now also runs
`extract` (a `serde_json::from_slice` parse) on the async executor, but
`INLINE_ZSTD_OUTPUT_BYTES` (32 KiB) and its ~100 µs Tokio-budget justification were
measured for decode alone, pre-fusion (`src/compression.rs:77-84`). Possible doc/perf
drift, not correctness — worth a follow-up measurement if this path gets busy.

**Why this matters for future review:** this project's authors write unusually thorough
doc comments that pre-empt review findings (e.g. explicitly marking the identity/Other
parse branches as "pre-existing, out of scope" and citing exact measured µs numbers).
When reviewing later rounds on this file, check whether a raised concern is already
addressed by an existing doc comment before flagging it as new.
