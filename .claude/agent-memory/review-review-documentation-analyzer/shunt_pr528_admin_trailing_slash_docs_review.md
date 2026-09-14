---
name: shunt_pr528_admin_trailing_slash_docs_review
description: shunt PR #528 (admin-trailing-slash, closes #527) doc-vs-impl review — clean pass, verified 18-path/20-pair count by hand against admin_router() and the ADMIN_PATHS table
metadata:
  type: project
---

PR #528 adds `/admin/` beside `/admin` in `admin_router()` (both → `dashboard()`)
because a `{*path}` wildcard can't match the empty string. Docs touched: `ui/README.md`,
`docs/admin-ui-delivery.md`, `site/src/content/docs/reference/endpoints.md` + ko/ja/zh-cn
mirrors. Zero findings — every changed sentence verified true:

- Hand-counted `admin_router()`'s base routes (no `--features ui`): 18 distinct paths,
  20 method+path pairs (the two multi-method routes are `/admin/login` GET+POST and
  `/admin/api/accounts/codex` GET+POST). Matches both the doc's "18 paths ... 20
  method+path pairs" and `tests/router_surface.rs`'s `ADMIN_PATHS` table exactly.
- `dashboard()` is `#[cfg(feature = "ui")]`-gated (shell vs 404-naming-the-feature) and
  both `/admin` and `/admin/` route to it, so "the only admin paths whose *answer*
  depends on the feature" is accurate — `/admin/api/*` is feature-invariant, and
  `/admin/assets/{*path}`/`/admin/{*path}` don't exist at all without the feature
  (different failure mode, correctly left out of that claim).
- `tests/admin_ui.rs` adds a positive-shell-body assertion for `/admin/` (not just a
  200/404 status check) — good regression coverage per [[live-capture-beats-mock-fixtures]]
  style reasoning (routing bug needs a routing-level assertion).
- All 4 locale endpoints.md copies got equivalent edits (table row + prose + a new
  2-paragraph split in the "Admin path migration" section); no locale gained a new
  cross-page fragment link (the only fragment links present, to `configuration.md`
  anchors, predate this diff and aren't English-anchor into another locale).
- Comment-rot sweep (`docs/m9-admin-surface.md`, `docs/desktop-app.md`, root README ×4)
  found no other route-count or `/admin/`-404 claims — this repo only states that
  fact once, in `docs/admin-ui-delivery.md`'s table, so there was nothing left stale.

Method note: counting `admin_router()`'s `.route(...)` calls by hand and cross-checking
against `tests/router_surface.rs`'s own `ADMIN_PATHS: [(&str, &str); N]` table is fast
and catches off-by-one route-count claims directly — no live probe needed for static
route inventories like this one (contrast with [[shunt_pr289_tool_search_default_review]]
where a live-probe claim needed checking against the implementing PR).

Restored after a concurrent reviewer agent deleted this file while cleaning up its own
scratch state; the `ADMIN_PATHS` spellings above were `ADMIN_ROUTES` in the original and
are corrected here — no such symbol exists in `tests/router_surface.rs`.
