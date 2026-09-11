---
name: shunt-pr526-admin-spa-cutover-docs-review
description: PR #526 (admin dashboard -> embedded SPA cutover) doc review method and the two gaps it missed; useful for future admin-surface behavior-change PRs.
metadata:
  type: project
---

PR #526 flips `GET /admin` from the deleted server-rendered dashboard to the
embedded React SPA shell (unauthenticated 200; sign-in redirect moves
client-side after a 401 from `GET /admin/api/session`), and makes `GET /admin`
without `--features ui` answer 404 naming the feature (route stays registered
in both builds). All 8 primary doc surfaces the PR touched (README x4 locales,
site/reference/endpoints.md x4 locales, docs/admin-ui-delivery.md,
docs/m9-admin-surface.md banner, ui/README.md, Rust doc comments) verified
accurate against `src/admin/mod.rs::dashboard`, `src/admin/ui.rs::shell`, and
`tests/router_surface.rs`'s new `the_mount_root_without_the_ui_feature_...`
test. ADR `.please/docs/decisions/0003-admin-dashboard-extension.md` correctly
left unedited (immutable).

Two gaps the PR itself left, found by grepping beyond the diff:
1. `site/src/content/docs/guides/admin-remote-provisioning.mdx` (+ko/ja/zh-cn)
   tells the reader to "Open `/admin` and sign in" with no mention that this
   now requires a `--features ui` build — the one guide page whose whole
   subject is this route, missed while `getting-started/installation.mdx`
   already had the feature caveat (from an earlier PR, #503-era).
2. The README "Optional server features" table's own fix (qualifying the Admin
   web surface row with the `--features ui` requirement) didn't extend to the
   "Upstream status polling — Statuspage indicators **in the dashboard**" row,
   which has the identical unstated dependency (dashboard indicators live in
   the SPA now; `ui/src/__tests__/upstream-status.test.tsx` confirms). Same
   defect class, same table, one row fixed and a sibling row with the same
   shape left as-is — worth checking every row sharing a keyword ("dashboard")
   with the row that got fixed, not just the row the PR touched.

Also noted (lower confidence, pre-existing / out of scope of this PR's own
prior touch): `docs/desktop-app.md` (draft Tauri design doc, PR #235, never
updated by the #499 namespace-split PR either) still asserts "`/admin`
redirects to `/admin/login`" as current server behavior — stale after this
PR's client-side-redirect change, but the file already had this drift habit
before #526 landed.
