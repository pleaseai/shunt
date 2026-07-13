---
name: pr85-admin-surface-error-handling
description: PR #85 (M9 admin web surface, src/admin/mod.rs) systematically discards real errors on every fallible admin-surface handler without logging, breaking the codebase's own tracing convention.
metadata:
  type: project
---

PR #85 (branch `amondnet/77`, opt-in `[server.admin]` browser account
provisioning + read-only pool dashboard) introduces `src/admin/mod.rs`. Every
handler that can fail (spawn_blocking store I/O, PKCE URL build, OAuth code
exchange, token storage, account removal) collapses the real `Err`/`JoinError`
into a generic HTTP error via `_ => internal("...")` / `Err(_) => bad_gateway(...)`
with **zero `tracing::warn!`/`tracing::error!` call** — even though the same
file uses `tracing::info!` for the success path (account add/store/remove), and
the rest of the codebase (`src/reload.rs`, `src/adapters/anthropic.rs`,
`src/adapters/responses.rs`, `src/main.rs`, `src/telemetry.rs`) consistently
logs `%error` before returning a generic response.

Worst instance: `complete_account` (src/admin/mod.rs:447-453) discards the
`store_setup_token` error entirely after a **single-use** OAuth code exchange
already succeeded — the code cannot be replayed, and there is no log entry
anywhere explaining why persistence failed (disk full, permission denied,
serialization error).

Also: `pool()` (src/admin/mod.rs:326) does
`claude_store::scan_accounts().unwrap_or_default()` — a real scan error (e.g.
permission-denied on the accounts dir) is silently presented to the operator
as "zero accounts" in the dashboard, not as an error.

**Why:** this is a fresh module (not pre-existing code), so every one of these
is in-scope for review, and the pattern is systemic across the whole file
rather than a one-off. `docs/m9-admin-surface.md` only documents that the
*token value* must never be logged (security-conscious, correct) and that
add/remove is "audit-logged by name only" on success — it says nothing about
logging failures, suggesting this was an oversight rather than a deliberate
security tradeoff.

**How to apply:** when reviewing future PRs touching `src/admin/`, check
whether this was fixed (add `tracing::warn!`/`error!(%error, ...)` before each
generic error response). If unfixed, re-flag — this is a durable, systemic gap
until addressed, not a one-time nit. Also worth checking: `read_account_meta`
in `src/auth/claude_store.rs:139-140` mirrors the pre-existing
`read_account_uuid` "swallow read/parse error to None" pattern, but now feeds
the admin dashboard's account list via `filter_map` — an unreadable/corrupt
account file silently disappears from the list rather than surfacing.
