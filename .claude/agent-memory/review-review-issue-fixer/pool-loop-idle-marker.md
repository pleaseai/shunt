---
name: pool-loop-idle-marker
description: Account-pool loops classify every Err by status; caller-owned body markers (idle) must short-circuit first. Seam and gotchas for testing it.
metadata:
  type: project
---

Adapter account-pool loops (e.g. gemini `forward`) classify each `Err` by HTTP status, and they reach that classifier before a caller-owned bound marker (`UpstreamBodyIdle`) is checked. The idle error is a 502, which classifies as Rotate, so the loop re-spends the bound on the next account.

**Why:** found in the PR that applied gated_idle_ms to the Gemini adapter (2026-09-24). `idle_error` promises `failure: None`, but the pool loop is a second failover layer inside the adapter that ignores that field.

**How to apply:**
- `body_idle()` lives on `proxy::ForwardError`, not on `AdapterError`. Inside an adapter, check `error.response.extensions().get::<crate::adapters::UpstreamBodyIdle>().is_some()` instead.
- Cheap e2e seam: the tests/buffer_replay gated harness plus `SHUNT_ANTIGRAVITY_ACCOUNTS_DIR` set to two account files, with a raw TCP listener that counts accepted connections. The upstream must be `kind = Antigravity`, because `antigravity_oauth` on `kind = Gemini` fails validation with `AntigravityOauthWrongKind`.
- Related: [[shunt-judge-byte-cap-class]], [[adapter-param-dropped-on-sibling-paths]].
