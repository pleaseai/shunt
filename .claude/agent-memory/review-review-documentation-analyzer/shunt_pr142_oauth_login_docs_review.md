---
name: shunt-pr142-oauth-login-docs-review
description: PR #142 (refreshable Claude OAuth login, --mode oauth/--manual, admin mode toggle) doc-vs-impl review — zero findings, clean pattern for future --mode-style CLI additions.
metadata:
  type: project
---

PR #142 on amondnet/claude-oauth-login added `shunt login claude --mode oauth|import|setup-token`
(+ `--manual`, deprecated `--long-lived` alias) and an admin-web `mode: "oauth"|"setup_token"`
toggle. Reviewed the full diff (README, docs/running.md, docs/m8-anthropic-multi-account.md,
docs/m9-admin-surface.md, site/**/{guides/admin-remote-provisioning,guides/anthropic-multi-account,
reference/cli,reference/configuration,reference/endpoints}.md × en/ko/ja/zh-cn) against
src/main.rs, src/auth/claude/{login,callback,auth,store}.rs, src/admin/{mod,session,html}.rs.

Result: zero findings. Notable things that could have been drift but weren't:
- A full-OAuth-provisioned account is stored via `store_oauth_tokens` and reports
  `AccountKind::Imported` (admin UI shows credential kind `imported`, not a distinct "oauth"
  kind) — this quirk is explicitly documented in admin-remote-provisioning.md across all 4
  locales rather than left implicit.
- Inserting a new docs/running.md §3.3 renumbered the old §3.3→§3.4; both intra-file §-refs and
  the `../site/...cli.md` relative link were updated correctly and resolve.
- Admin API's `AddMode` default (`SetupToken`, serde snake_case) matches every doc's claim that
  omitted `mode` defaults to `setup_token` while the dashboard UI defaults to `oauth`.

Method note: for a --mode-style CLI addition spanning 4 locales, the fast check is: diff the EN
source doc against src/main.rs's clap enum + tests first (ground truth), then diff each locale
file against the EN diff hunk-for-hunk to confirm flag spellings/mode values match (not
prose/translation quality) — this PR's ko/ja/zh-cn were all mechanically parallel to EN.

See also [[shunt_pr136_pool_lb_docs_review]] for the previous cross-provider struct-doc-trap
finding on this repo, and [[shunt_retry_docs_fan_out]] for the fan-out pattern this repo tends
to use for behavior documented in 4+ places at once.
