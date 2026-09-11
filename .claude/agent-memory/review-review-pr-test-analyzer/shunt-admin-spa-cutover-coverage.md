---
name: shunt-admin-spa-cutover-coverage
description: PR #526 (admin SPA cutover, deletes html.rs dashboard_page + its 15 tests) gap analysis
metadata:
  type: project
---

PR #526 deletes `src/admin/html.rs`'s `dashboard_page` and all 15 of its
`mod tests` (matched emitted JS *source text*), flipping `GET /admin` to the
embedded React SPA shell. Verified all 15 deleted properties against
`ui/src/__tests__/*` (mostly pre-existing from #508: `layout`, `coalescing`,
`claude-accounts`, `codex-accounts`, `provisioning-races`, `mutations`) plus
two new files this PR adds (`empty-usage.test.tsx`, `upstream-status.test.tsx`).
Every property maps to a real behavioral (RTL) test; none are vacuous. The
`empty-usage.test.tsx` `it.each` genuinely reaches all 4 `emptyUsageText`
branches, correctly deriving `state` through `effectiveState`/`observedRow`
(traced by hand — `observation.state` flows straight into `row.state` when
there's no managed match, so the fixture's `state`/`signal` overrides land on
the branch the test names).

One real gap found: `tests/router_surface.rs`'s new
`the_mount_root_without_the_ui_feature_explains_the_missing_bundle` is
`#[cfg(not(feature = "ui"))]`, but `.github/workflows/ci.yml` has exactly one
test job and it always runs `cargo test --all-features --workspace` (which
turns on `ui`). Confirmed by direct build: the test compiles and passes when
run locally without `--all-features`, but per grep of the workflow file there
is no job that ever invokes `cargo test` without `--all-features`, so this
`#[cfg]`-gated test body is never compiled in CI — pattern also present
pre-existing in the same file (`router_surface.rs:498,547,615`), so it's a
known/accepted repo-wide gap (see `docs/admin-ui-delivery.md`'s
default-build-has-no-dashboard resolution), not something newly introduced,
but still worth flagging per-PR since new `cfg(not(feature="ui"))` tests keep
being added under the same CI blind spot.

The `upstream-status.test.tsx` omission of a "failed status read hides the
section" test is *correctly* reasoned as untestable via DOM: traced
`useDashboard.ts` — a failed `readJson` resolves `sources = []` (via
`result.ok ? ... : []`) then `setStatus(sources.length ? sources : null)` →
`null`, identical to both "not configured" (initial state) and "still
pending". No DOM observation distinguishes the three; correctly left
untested rather than faked.

`admin_surface.rs`'s two tests moved from asserting `GET /admin` 200 (now
meaningless post-cutover since the shell is unauthenticated) to
`GET /admin/api/session` 200/401 — a genuine strengthening, not weakening;
confirmed `session_bootstrap` still 401s a logged-out/no-cookie caller.
