---
name: project-shunt-pr103-i18n-comparison-review
description: PR #103 (docs(site) translate multi-account/admin guides, sync stale i18n, add comparison page) doc-vs-source review — clean pass, zero findings.
metadata:
  type: project
---

Reviewed PR #103 on pleaseai/shunt (`git diff origin/main...HEAD`, 21 files, +1252, all docs/site) — a
translation-heavy PR: new ko/ja/zh-cn translations of `guides/anthropic-multi-account.md` and
`guides/admin-remote-provisioning.md`; sync updates to 9 stale translation files
(`{ko,ja,zh-cn}/guides/shared-gateway.md`, `.../reference/configuration.md`, `.../reference/endpoints.md`);
a new `getting-started/comparison.md` (+3 translations) adapted from repo-root `docs/comparison.md`;
`astro.config.mjs` sidebar entries; `package-lock.json` engines sync.

Result: zero documentation-accuracy findings. Every numeric/literal fact in the watch list (0.98 switch
threshold, 3600/600s TTLs, 60s/1s/5min/30s cooldowns, 1–3600s/300s clamps, header names, route paths,
TOML keys, file:line citations, GitHub issue numbers/URLs) matched exactly across the English source and
all three locale translations. `docs/comparison.md` → site adaptation introduced only the pre-authorized
editorial deltas (frontmatter, H1 removal, scope note as `:::note` aside, one feature-matrix cell
`○`→`◐ (opt-in admin surface)`, §5 bullet rewrite citing the admin surface + site links instead of
`src/server.rs:69-75`) with no unauthorized factual drift — confirmed via full `diff` against the source
doc. All locale-prefixed internal links (`/ko/...`, `/ja/...`, `/zh-cn/...`) correct throughout, including
in the new comparison.md translations' §4-7 prose and footnotes. `astro.config.mjs` sidebar additions
(new "Comparison" entry + newly added translations on the pre-existing "Admin & Remote Provisioning"
entry) cross-checked against each translated page's frontmatter `title` — all matched.

Confirms the pattern already seen in [project_shunt_m9_admin_docs_review](project_shunt_m9_admin_docs_review.md):
this repo's doc-writing/translation practice is consistently accurate. Two PRs in a row (admin surface
impl docs, then i18n translation of the same feature) have produced zero doc-accuracy bugs. Still verify
every specific claim independently per review methodology — but calibrate initial expectations accordingly.

One reusable technique: for large translated Markdown files where a full `diff` against the English
source produces output too large to review in full, `grep -n "^|"` (table rows) plus targeted `grep -n`
for footnote markers / file:line citations / issue-link patterns (`issues/NN`, `src/....rs`, `docs/....md`)
gives high-confidence coverage of the highest-risk numeric/factual content without needing the full diff
text in context.
