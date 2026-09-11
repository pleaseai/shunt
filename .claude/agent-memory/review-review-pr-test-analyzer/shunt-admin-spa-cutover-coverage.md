---
name: shunt-admin-spa-cutover-coverage
description: PR #526 (admin SPA cutover, deletes html.rs dashboard_page + its 15 tests) coverage analysis; all 15 properties land on real RTL tests and the cfg(not(ui)) route test is run by ci.yml's default-build step
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

No CI gap, and the reason is worth keeping: `tests/router_surface.rs`'s new
`the_mount_root_without_the_ui_feature_explains_the_missing_bundle` is
`#[cfg(not(feature = "ui"))]`, and `.github/workflows/ci.yml`'s main test step
always runs `cargo test --all-features --workspace`, which turns `ui` on and
compiles that arm out. This PR closed that blind spot in the same commit: a
second step, `Test default build (no ui feature)`, runs
`cargo test --test router_surface` with default features, and `ui` is not a
default feature (`Cargo.toml`), so this test (`router_surface.rs:615`) and the
two pre-existing `#[cfg(not(feature = "ui"))]` arms beside it
(`router_surface.rs:498,547`) are compiled and executed in CI.

The remaining exposure is the step's scope, not this test: it names one test
binary. A `#[cfg(not(feature = "ui"))]` arm added to any *other* test file is
still dead source as far as CI is concerned until that binary is named in the
step too — which is what the step's own comment says.

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
