---
name: codex-routing-hint-header
description: PR #403 x-codex-routing-hint — client-tainted string on the Codex path; round-2 fix made it Option<HeaderValue> (len/`;`/HeaderValue guards), residual = denylist-of-one grammar guard.
metadata:
  type: project
---

`x-codex-routing-hint` (`model=<upstream_model>[;tier=<tier>]`) is emitted on the
ChatGPT-OAuth arm only, at exactly two forward sites:
`src/adapters/responses/request.rs` (`routing_hint()`) and
`src/adapters/responses/websocket.rs` (`websocket_headers`). The inbound Codex
passthrough (`inbound.rs`, `passthrough_request_headers` — site 3) never adds it
and relays the caller's own **verbatim** (`PASSTHROUGH_STRIP_REQUEST_HEADERS`
does not list it), by design.

**Why:** `route.upstream_model` equals the client's raw `model` field under
prefix-route and default-provider routing (`routing.rs::route_for`), only `[1m]`
stripped. `route.service_tier` by contrast is config-only and enum-validated, so
the `;tier=` segment is never client-tainted.

**How to apply (round-2 fix, verified 2026-08-19):** `routing_hint` now returns
`Option<HeaderValue>` and both sites omit rather than set. Closed: `;`-forgery,
unbounded length (`> 128` bytes, inclusive bound, mirrors
`observability::MAX_MODEL_TAG_LEN`), and the deferred-reqwest-builder-error →
`pool.rs` 30s per-account "transport" cooldown (`HeaderValue::from_str(..).ok()`).
The *other* client-controlled header on that path, `session_id` /
`x-codex-window-id`, cannot produce a builder error because it comes from a
parsed inbound `HeaderMap` via `to_str()` (visible ASCII only) — no second
cooldown vector. Residual: the grammar guard is a **denylist of one character**;
`,`, SP, TAB, `=` all still pass into the value, so any server-side parser that
is not strictly `;`-then-`=` is unproven. A positive slug allowlist would close
it for good.
Related: [[m11-inbound-codex-endpoint-security]], [[shunt-unbounded-model-label]].
