---
name: shunt-pr136-pool-lb-docs-review
description: PR #136 (per-account thresholds + burn-rate pool LB, issue #135) doc review — two real findings and the shared-AccountConfig cross-provider doc trap.
metadata:
  type: project
---

PR #136 added `[server.pool]` (hard_threshold/default_threshold*/burn_rate_avoidance)
and per-account `threshold*`/`priority`/`disabled` to `AccountConfig`. Reviewed
README.md, docs/m8-anthropic-multi-account.md, shunt.toml.example,
site/reference/configuration.md + guides/anthropic-multi-account.md (en/ja/ko/zh-cn)
against src/config.rs + src/accounts.rs. Almost everything matched precisely
(threshold resolution order, hard-threshold cap, burn-rate headroom formula,
all-near guard, admin /admin/pool fields) — this PR's docs were unusually careful.

Two real findings:

1. **Shared-struct cross-provider doc trap.** `priority`/`disabled` live on
   `AccountConfig`, a struct shared by both Claude (`claude_oauth`) and Codex
   (`chatgpt_oauth`) pools, and `select_order`'s priority-sort + disabled-filter
   run unconditionally regardless of the `pool: Option<&PoolConfig>` argument —
   so these two keys are fully live on Codex pools today, confirmed by
   `src/adapters/responses/{inbound,pool}.rs` comments ("per-account
   priority/disabled still apply") and by README/m8-doc/site-reference-config
   explicitly saying "Applies to Claude and Codex pools alike" / "Applies to
   Codex pools too." But **docs/m10-codex-multi-account.md** and
   **site/guides/codex-multi-account.md** (English source, so no locale mirror
   has it either) still list only `credentials`/`token_env`/`uuid` in their
   Account fields table — `priority`/`disabled` are undocumented on the only
   pages a Codex-pool operator would read. General lesson: when a PR adds a
   field to a struct shared across providers, grep for the struct name across
   *all* per-provider doc files, not just the provider the PR's own commit
   message names.

2. **Dangling forward-reference in locale reference pages.** English
   `site/reference/configuration.md` has a `### [[providers.<name>.accounts]]`
   subsection; ja/ko/zh-cn `reference/configuration.md` do **not** (pre-existing,
   confirmed via `git show main:...`, not introduced by this PR) — those locales
   document accounts only on the guide page instead. This PR's new
   `[server.pool]` section text was translated verbatim including "...the
   per-account `priority` and `disabled` keys below still apply" — accurate in
   English (the accounts subsection really is below), but dangling in ja/ko/zh
   since there's no accounts section on that page at all. Low-severity but a
   real reader-facing issue: check literal "below"/"above" self-references
   survive translation onto pages with a different section structure than the
   English source.

See also [[shunt_retry_docs_fan_out]] (same "grep every doc surface, not just
the one the PR touched" lesson from PR #126) and
[[shunt_i18n_docs_structure]] for the ko/ja/zh-cn layout.
