---
name: shunt-admin-trailing-slash-coverage
description: PR #528 tests/admin_ui.rs+router_surface.rs coverage for the /admin/ trailing-slash fix; confirms inventory-driven tests auto-cover HEAD/405/Allow for a newly-added route
metadata:
  type: project
---

PR #528 fixed `GET /admin/` 404ing (axum `{*path}` can't match empty string) by
registering `/admin/` beside `/admin`, both → `dashboard()`. Reviewed against
[[route-inventory-gate-blind-spots]].

Findings, all confirmed by direct experiment (delete the `.route("/admin/", ...)`
line, rerun, restore):

- The author's non-vacuity claim held for all 4 touched assertions: `the_mount_root_with_a_trailing_slash_serves_the_spa_shell` (404 vs 200), the feature-off loop test (empty body vs `--features ui` text), `every_registered_method_set_matches_the_inventory` (404 vs 405 on the `ADMIN_PATHS` PATCH probe), and `the_source_scan_finds_every_literal_registration` (40 vs 41 literal-`.route("` count from `include_str!`). All four failed with a real assertion mismatch, not a compile error.
- `dashboard()` is a direct passthrough to `ui::shell()` — the *same* function object registered at both `/admin` and `/admin/` — and `ui::shell()` hardcodes its CSP/security headers as literal response headers, not derived from the request path or any per-path middleware layer (`src/server.rs`'s two `middleware::from_fn_with_state` layers — concurrency limit, http tuning — are both path-agnostic). So "one hardening test on `/admin` covers `/admin/` too because they're the same handler" is sound; there is no second code path to miss.
- Once a path is added to `ADMIN_PATHS` (the `router_surface.rs` inventory table) with its method set, `every_registered_method_set_matches_the_inventory` automatically drives a real PATCH-probe 405+`Allow` check against it, and the router's own `HEAD` auto-add for `GET` is folded into the `"GET,HEAD"` method-set string. **A dedicated per-route HEAD/405 test is redundant once the route is in the inventory table** — this is a recurring pattern worth checking on future single-route-addition PRs before flagging "no HEAD/405 test" as a gap.
- One real, but *pre-existing and out-of-diff*, edge case found by live probe: `/admin//` (double slash) returns `200 text/html` — it falls into the `/admin/{*path}` catch-all with `path="/"` , same as `/admin/anything`. This behavior predates PR #528 (the catch-all already existed) and is unaffected by adding the `/admin/` exact route, so it is correctly out of scope per this repo's "nothing about code this diff doesn't change" filter — noted here in case a future PR touches the catch-all and needs the context.
