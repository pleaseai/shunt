---
name: shunt-issue363-slots-audit-pr391
description: PR #391 (issue #363) auth/slots.rs credential-slot enumeration refactor — reviewed clean, verified HeaderMap::remove semantics
metadata:
  type: project
---

PR #391 introduced `src/auth/slots.rs` (`ShuntCredentials::{from_state, strip_reserved_slots,
strip_consumed_slots}`) as the single enumeration of shunt's own inbound-credential slots, and
routed the three forward sites (`proxy::failover`, `discovery::upstream::upstream_headers`,
`adapters::responses::inbound::passthrough_request_headers`) through it. This closes out the
saga tracked in [[new-credential-kind-slot-audit]] (4 prior point-fixes: #352, #357, #361, #356).

**Reviewed clean** (2026-08-18): hunk-by-hunk diff against `origin/main`, full test suite green,
`cargo fmt --check` + `clippy -D warnings` clean, no new `unwrap`/`expect`/panic in request paths.

**Verified facts worth keeping**:
- `http::HeaderMap::remove` (checked crate source at `~/.cargo/registry/.../http-1.4.2/src/header/map.rs:1585`)
  removes **all** values for a key, not just the first — it calls `remove_all_extra_values`
  before returning the first. So a forward site that does `out.append(...)` in a copy loop
  (preserving multi-value headers) and then `strip_reserved_slots` → `HeaderMap::remove` on the
  result does NOT leak a second value of a stripped header. This was a specific worry in the
  review brief and is a non-issue for any future PR touching this code — don't re-derive it,
  just cite the doc comment above `remove()`.
- `is_consumed_by_shunt` was gated `#[cfg(test)]` in this PR; grepped all call sites
  (`src/proxy/failover/tests.rs`, `src/discovery/upstream/tests.rs`, `src/gateway/tests.rs`) —
  all are themselves `#[cfg(test)]` modules, so the gating doesn't break any non-test/feature build.
- Two deliberate behavior widenings, both documented in the module doc and in
  `docs/m4-inbound-auth.md` §2/§2.1: (1) `RESERVED_SLOTS` (`x-shunt-token`, `x-shunt-admin-token`,
  `x-shunt-inbound-client`) are now stripped unconditionally instead of only-when-configured;
  (2) the Codex passthrough endpoint now also strips the `[server.admin]` header (previously
  leaked it — this was the actual bug issue #363 targeted).
- `every_bulk_header_forward_is_a_registered_site` tripwire test (walks `src/**/*.rs` for bulk
  header-map application patterns, asserts against a hard-coded allowlist) passed and is a good
  regression guard against a 5th recurrence of this defect class — worth checking still exists
  and still passes in any future PR touching header-forwarding code.

No findings reported (empty findings array). If a future PR touches this area, start from
`src/auth/slots.rs`'s own module doc comment — it is unusually complete (states the mirror
invariant, lists every accept site, every forward site, and the tripwire) and pre-empts most
review questions.
