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
