---
name: project-shunt-m9-admin-docs-review
description: PR #85 (amondnet/77, M9 admin web surface) doc-vs-implementation review outcome and the one real finding
metadata:
  type: project
---

Reviewed PR #85 on pleaseai/shunt, branch `amondnet/77`, adding the opt-in `[server.admin]` web
surface (src/admin/{mod,html,session}.rs + docs across README.md, docs/m9-admin-surface.md,
shunt.toml.example, and 5 site/src/content/docs/{guides,reference} files).

**Why this is worth remembering:** this PR is a strong positive example of doc/code discipline in
this repo — every config key, default, endpoint, security claim (cookie Secure-except-loopback,
CSRF same-origin+token, session/pending TTLs, rate-limit, audit-log-by-name-only, fail-closed
startup) cross-checked exactly against src/config.rs, src/admin/*.rs, src/server.rs, src/reload.rs,
src/accounts.rs::snapshot, src/auth/claude_store.rs, src/auth/claude_login.rs. Zero doc-accuracy
bugs found across ~700 lines of new docs. Route tables in docs/m9-admin-surface.md and
site/.../reference/endpoints.md matched `admin_router()` in src/admin/mod.rs verbatim, including
method combinations (`GET,POST /admin/login`).

**One real finding (comment rot, not user-facing docs):** `src/auth/claude_store.rs`
`read_account_meta()` (new in this PR) has a doc-comment claiming account kind is derived from
"the explicit kind marker" (i.e. `shuntCredentialKind`) plus refresh-token presence ("Fall back to
setup-token when neither signal is present"), but the actual code only checks `refreshToken`
presence — `shuntCredentialKind` is never read in that function. Behavior is still correct (single
signal is sufficient), just the comment overstates the logic. Confidence ~70, severity minor.

**Pre-existing gap noticed but correctly NOT flagged (out of diff scope):**
`site/src/content/docs/reference/endpoints.md` has never listed `GET /protocol` in its table —
true on `origin/main` before this PR too, so left out of findings per the "pre-existing issues
outside diff scope" rule.

**Method that worked well:** for a security-surface PR like this, walk every specific/numeric claim
(header names, env var names, defaults, TTL seconds, route paths+methods, regex patterns like
`[a-z0-9-]+`, scope strings like `user:inference`) in the prose docs against a `grep`/`Read` of the
exact implementing code — this repo's docs tend to be written by someone who already read the code
closely, so the yield of real findings is low but the numeric/literal claims are exactly the kind
that silently drift and are worth the systematic pass every time.
