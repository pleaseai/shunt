---
name: pr291-zstd-compression-failopen
description: PR #291 (issue #285) zstd request compression — deliberate fail-open paths in prepare_body/model_label, and the decode-budget invariant that does not hold
metadata:
  type: project
---

PR #291 adds zstd Responses **request** compression (`src/compression.rs`, `src/adapters/responses/body.rs`),
default ON per provider but gated to the ChatGPT/Codex flavor, plus zstd decoding of the inbound
`[server.codex_endpoint]` body just to read the `model` metrics label.

Findings worth remembering:

- `decode_zstd_within` picks inline-vs-offload on the **compressed input** size (`INLINE_ZSTD_BYTES` = 64 KiB),
  but the budget is the **decoded** cap. `codex_endpoint::model_label` passes `MAX_REQUEST_BODY_BYTES` (64 MiB),
  so a 64 KiB zstd body can decompress up to 64 MiB inline on the async executor — for a log label only.
  The doc comment claiming "a compressed body cannot make shunt buffer more than an uncompressed one would"
  is false: the arrival bytes and the decoded copy are resident at the same time.
- `parse_model` still does `serde_json::from_slice(..).ok()`; the fix surfaces *that* the model is unreadable
  but never *why*. Malformed JSON and valid-JSON-without-`model` share one warn line.
- `prepare_body`'s compression fail-open (warn + send uncompressed) has no counter, unlike the sibling
  fail-open path that got `shunt.codex_ws_overflow` in b5b7ec4. Fail-open + metric is the house pattern.
- The only positive trace that a request was compressed is `tracing::debug!` — an upstream/middlebox
  rejection of the new default-on encoding yields an error with no hint the body was zstd.

**Why:** these are the exact places a future regression would hide (label reverts to `unknown`, or every
turn silently stops compressing).
**How to apply:** when reviewing later changes to `src/compression.rs`, `body.rs`, or `codex_endpoint.rs`,
re-check the offload/budget asymmetry and whether new `Ok(None)` "skip" arms are distinguishable at call sites.
Related: [[pr125-codex-passthrough-endpoint]], [[pr272-cursor-offload-errors]].

## Iteration 2 (163d50e) — verified clean
All iteration-1 findings (decode offload keyed on compressed size vs decoded budget;
`parse_model` using `.ok()`; fail-open with no counter) were fixed correctly:
- `decode_zstd_within` now probes inline against `min(INLINE_ZSTD_OUTPUT_BYTES, budget)`,
  and on `Ok(None)` from the probe falls through to an offloaded re-decode against the
  real `budget` — that second call's `Ok(None)` is the authoritative over-budget answer.
  Malformed zstd still propagates as `Err` via the inline branch's `?` (verified: it does
  NOT get masked as `Ok(None)`).
- `ParsedModel` enum (Model/Malformed/Missing/NotAString) replaces `.ok()`; all four
  variants reachable and each logged via `tracing::warn!` before degrading to "unknown".
- `prepare_body` borrow-not-clone in pool.rs confirmed: exactly one call site each in
  pool.rs (lazy, memoized in `Option<PreparedBody>`) and http.rs (upfront); doc comment
  matches code exactly.
- No stale `INLINE_ZSTD_COMPRESS_BYTES` refs left anywhere.
- New tests (inbound_codex_endpoint::forwards_a_zstd_compressed_body_verbatim,
  codex_multi_account::refresh_retry_and_rotation_reuse_the_identical_compressed_body,
  config/upstreams/tests.rs::request_compression_defaults_true_and_can_be_disabled) are
  non-vacuous and pass; env-var isolation uses REFRESH_ENV_LOCK + unique temp dirs, no
  collision with sibling tests.
Verdict: 0 findings, build+clippy+targeted tests all green.
