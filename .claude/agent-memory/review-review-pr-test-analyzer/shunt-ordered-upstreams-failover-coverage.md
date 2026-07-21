---
name: shunt ordered-upstreams-failover-coverage
description: PR #224 ordered cross-provider failover coverage map; post-2xx chain stop remains unguarded, presets partly asserted
metadata:
  type: project
---

PR #224's `tests/failover.rs` thoroughly covers chain order, advance statuses, connect failures, immediate 400 stop, best-failure selection, synthesized exhaustion, Responses raw-status propagation, whole-chain inbound gating/credential stripping, count_tokens pinning, and gateway metadata headers. Config/routing tests cover mixed declaration rejection, legacy multi-map rejection, order filtering, OAuth whole-store/subset resolution, and physical-account key behavior. The critical remaining gap is an end-to-end two-upstream test where the first returns 2xx headers and then its body/stream fails, asserting the second receives zero requests; existing Codex WebSocket tests establish the local post-first-event boundary only in a one-element route. Preset tests assert all names but full fields only for Kimi (plus partial OpenAI/Codex paths), leaving several credential-bearing preset contracts unguarded.

**Why:** Cross-provider replay after response commitment can duplicate a non-idempotent LLM turn, while adapter-local no-fallback tests do not prove the newly introduced outer chain cannot advance.

**How to apply:** Future failover reviews should require an explicit second-upstream zero-hit assertion for every response-commit boundary and compare table-driven presets field-by-field against the documented contract.
