---
name: shunt-inbound-codex-endpoint-coverage
description: PR #125 tests/inbound_codex_endpoint.rs gap analysis for the new raw-passthrough inbound Codex endpoint (src/adapters/responses/mod.rs forward_codex_passthrough*); recurring "new function duplicates an already-tested pool path but its own failover branches go untested" pattern.
metadata:
  type: project
---

PR #125 added `[server.codex_endpoint]` — a raw OpenAI Responses passthrough (`forward_codex_inbound` / `forward_codex_passthrough` / `forward_codex_passthrough_single` in `src/adapters/responses/mod.rs`) alongside 10 integration tests in `tests/inbound_codex_endpoint.rs`.

**Pattern to watch for on future shunt PRs**: this passthrough function is a near-duplicate of the already-well-tested outbound pool path (`forward_chatgpt_oauth`, covered by `tests/codex_multi_account.rs` — see [[shunt-codex-multi-account-coverage]]), reusing the same `classify_codex`/`refresh_lock`/`force_refresh` machinery. Because the *shape* is familiar, it's easy to assume coverage carries over — it doesn't; it's a distinct function with its own bugs to catch. Concretely, PR #125's new test suite covered the happy path, 3 routes, SSE relay, 429-rotate-to-exhaustion (verbatim relay), session-sticky, and both inbound-auth schemes — but had ZERO tests for:
- The 401 → `RefreshRetry` branch (force-refresh + retry, token_env-not-refreshable sub-branch, refresh-still-401 sub-branch) — this fires on every real OAuth token expiry, i.e. routine production behavior, yet was entirely unexercised.
- `forward_codex_passthrough_single` (the accounts-empty fallback) — likely the *most common* single-user deployment shape, since every test in the suite passed a non-empty `accounts` vec to `test_config`.
- The exhaustion-before-any-upstream-response `own_error` 502 path — and notably this path returns an **Anthropic-shaped** error envelope (`ShuntError::bad_gateway` → `{"type":"error",...}`) to a raw Codex CLI client, which is a real deviation from this endpoint's "verbatim relay" contract that a test would have caught/documented.

**Why to apply**: when reviewing new inbound/pooled-account passthrough code that reuses an existing pool's failover enum (`classify_codex`/`classify` style), always diff the new function's branches against the sibling function's test file rather than assuming "it's basically the same code, so it's basically tested." Check specifically: (1) the 401/refresh branch, (2) the zero-accounts fallback, (3) the all-accounts-failed-before-any-response terminal error shape.
