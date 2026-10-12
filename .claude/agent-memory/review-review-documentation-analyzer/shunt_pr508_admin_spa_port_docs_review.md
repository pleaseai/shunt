---
name: shunt-pr508-admin-spa-port-docs-review
description: PR #508 (admin-spa-port) docs review — zero findings; all 7 flagged claims verified true against code; contrasts with #503's 4 defects in the same track.
metadata:
  type: project
---

PR #508 ported the server-rendered admin dashboard into the React/Vite bundle
`--features ui` embeds, adding `GET /admin/api/session`. Reviewed against the
predecessor PR #503 in the same track, which had 4 doc defects (3 introduced
while fixing the prior one — each a claim true for one build config/method but
false for another).

All 7 specific claims verified TRUE against the implementation:
1. "17 paths / 19 method+path pairs" in `docs/admin-ui-delivery.md`'s Current
   surface table — cross-verified against `admin_router()` in `src/admin/mod.rs`
   and `ADMIN_PATHS` in `tests/router_surface.rs`. The nested "M9 documents 15
   of them" sub-claim is also correct: M9's table has 16 rows because two
   (`GET`/`POST /admin/accounts/codex`) collapse into one current path
   (`/admin/api/accounts/codex`), landing at 15 matched current paths + 1
   (`GET /admin/status`, pre-existing drift not in M9's table at all) + 1 new
   (`/admin/api/session`) = 17.
2. Decision 5's CORS claim ("no CORS layer anywhere on the admin router") —
   confirmed via `rg -i cors` (only the doc comment itself matches) and no
   `tower-http` cors feature in Cargo.toml.
3. `ui/README.md`'s "any other path under the mount (`/admin/ui`) already
   renders this bundle" — true only for GET (`/admin/{*path}` registered via
   `get(ui::shell)` only), but the README's own framing ("how to try it [via
   browser]") makes GET the implied method; not misleading enough to flag.
4. `src/admin/html.rs`'s `STYLE` const vs `ui/src/index.css` — diffed the raw
   CSS content (extracted the Rust string literal body) against the `.css`
   file: byte-identical aside from a header comment block in the `.css` file.
   "is a copy of this" holds, no drift introduced by this PR.
5. `src/admin/ui.rs` module doc's "`GET /admin` itself is untouched" — true;
   `.route("/admin", get(dashboard))` is registered unconditionally, outside
   the `#[cfg(feature = "ui")]` block.
6. Locale endpoint rows (`ko`/`ja`/`zh-cn` `reference/endpoints.md`) for
   `GET /admin/api/session` — all four present, semantically parallel, added
   in the same diff.
7. `ui/src/**` comment citations all resolve to real symbols: `Tokens::is_valid_at`
   (`src/auth/claude/auth.rs:253`, semantics match exactly —
   `expires_at_ms > now_ms + EXPIRY_BUFFER`), `PendingStore::attempt`
   (`src/admin/session.rs:225`, leaves entry in place on success; separate
   `remove` call after store — matches the ui comment's race description
   exactly), `observation::parse_claude` (`src/auth/observation.rs:1151`),
   `auth/codex/store.rs` (exists), `cleanup_reprovisioned_pool_health` (defined
   in `src/admin/mod.rs:431`, called from `src/admin/codex.rs:241` — the ui
   comment cites `src/admin/codex.rs` as if that's where the function lives;
   ambiguous but plausibly read as "call site", left unflagged).

Net: this PR, unlike #503, shows no doc/code drift — every symbol, path, and
behavioral claim added in the diff was independently verified true. Useful
contrast for pattern-tracking: a large diff with heavy prose-in-comments
density is not automatically defect-prone: this one's authors evidently
grepped/traced every claim before writing it.

No memory note exists for the #503 docs review, so the contrast above stands on
this note's own description of it (4 defects, 3 of them introduced while fixing
the prior one) rather than on a link.
