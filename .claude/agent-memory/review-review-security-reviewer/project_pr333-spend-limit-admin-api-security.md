---
name: pr333-spend-limit-admin-api-security
description: PR #333 gateway spend-limit Admin API posture — auth/route-gating/secret-redaction/persistence all verified safe; residuals = no auth throttle, id-only key-uniqueness, unbounded user_id/amount growth.
metadata:
  type: project
---

`src/gateway/spend/` (PR #333, branch `amondnet/spend-limit-api`) adds the first
authenticated **write** HTTP surface on the gateway router.

**Verified safe — do not re-flag without a code change:**

- `api.rs::authenticate()` uses `crate::auth::inbound::constant_time_eq` for every
  candidate key and never breaks early, so iteration count is config-dependent,
  not input-dependent. Read keys are matched first and write keys second, so
  write-last-wins; a read key can never satisfy `write_required` (403 branch).
- Fails closed on every absence: no `[server.gateway.admin]` → 401; empty key env
  → zero keys → 401; missing `x-api-key` header → 401. Routes are only registered
  when `admin.is_some()` at boot (`src/server.rs` `spend_admin_enabled` →
  `gateway::gateway_router(bool)`), and a reload that drops `admin` leaves the
  routes registered but `authenticate` then 401s.
- Auth runs **before** body/query extraction in both `create` and `list`, so an
  unauthenticated caller cannot reach the parsers.
- Secret redaction is real: `write_keys`/`read_keys` are `#[serde(skip)]` (blocks
  both Serialize and Deserialize) and `GatewayAdminConfig` has a hand-written
  `Debug` that renders `id:<redacted>`. Covered by a regression test that also
  asserts the nested `format!("{config:?}")`. Nothing serializes `Config` to a
  response or file (`src/dashboard.rs` edits raw TOML text).
- Persistence uses `crate::atomic_file::write_private_atomic` (0600, O_EXCL temp,
  dir fsync). Only caps + audit records land there — no key material.
- `restore()` runs in `src/main.rs::serve` **before** `axum::serve`, so its
  unconditional `SpendStore::replace` cannot clobber a live mutation.
- `list` `limit` is range-checked (1..=1000) *before* `paginate` does
  `start + query.limit`, so no overflow. Not CSRF-able (custom header, no CORS layer).

**Residual gaps (reported, low severity):**

1. No brute-force throttle on `authenticate()` — `src/admin/mod.rs:421` has
   `admin_stores.login_rate.check()` as explicit defense-in-depth behind its own
   constant-time compare; the spend API has no equivalent.
2. `GatewayAdminConfig::resolve_keys` rejects duplicate key **ids** only, not
   duplicate key **values**; combined with write-last-wins, the same secret listed
   under a read id and a write id silently grants write.
3. `user_id` (`parse_scope`) and `amount` (digit string) have no length bound, and
   every mutation appends a full before/after audit snapshot and rewrites the whole
   state file — unbounded disk growth for a write-key holder until the deferred
   retention sweep lands.

**How to apply:** re-open the auth analysis only if the loop order in
`api.rs::authenticate`, the `#[serde(skip)]`/`Debug` impl on `GatewayAdminConfig`,
the route-gating boolean in `src/server.rs`, or the restore ordering in
`src/main.rs::serve` changes.
