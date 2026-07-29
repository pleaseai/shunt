---
name: shunt-pr272-cursor-offload-doc-review
description: PR #272 Cursor CPU offload docs review found failover-spec drift from new framing errors; performance-only claims otherwise needed no docs.
metadata:
  type: project
---

PR #272's request-framing and large-image decode offloads add no config, endpoint, CLI, default, provider, model, or normal response-behavior change, and the cited #256 precedent accurately touched only `src/` and `benches/`. The useful review trap is error classification: new failures around pre-send CPU work may be mapped through an existing transport-error mapper and thereby contradict the ordered-failover specification's statement that Cursor adapter-owned errors return immediately.

**Why:** `cursor request framing: ...` is constructed before any upstream request but routed through `map_client_error`, which marks all such errors `AdapterFailure::BeforeHeaders`; ordered failover advances those failures, while the spec grouped Cursor adapter-owned errors with local errors that do not advance.

**How to apply:** On future Cursor internal/offload refactors, trace every newly possible local error through `AdapterError.failure` and compare it to `docs/upstreams-failover.md` §6, even when the PR is otherwise described as performance-only. Exact internal error strings do not need user docs unless an existing contract enumerates them.
