---
name: shunt-i18n-docs-structure
description: How shunt's Astro Starlight docs site is internationalized (ko/ja/zh-cn) and the verification method that worked well for translation PRs
metadata:
  type: project
---

pleaseai/shunt uses Astro Starlight with `defaultLocale: 'root'` (English lives at
`site/src/content/docs/<subpath>`, no `en/` folder) and locale subfolders `ko`, `ja`, `zh-cn`
mirroring the same relative paths. Root READMEs follow `README.md` / `README.ko.md` /
`README.ja.md` / `README.zh-CN.md` (note: directory is lowercase `zh-cn`, but the README file is
`zh-CN` — different casing between site locale key and README filename, by design, not a bug).

PR #52 (2026-07, branch amondnet/cjk) added all three translations in one PR: 45 files, sidebar
`translations{}` blocks in `site/astro.config.mjs`, and a language-switcher line
(`**English** · [한국어](README.ko.md) · ...`) added atop every README variant.

**Why:** For this kind of PR, translation wording is explicitly out of scope for doc review — only
structural fidelity to the English source matters (frontmatter keys, code blocks/commands/env vars
verbatim, internal links resolving, headings/sections not dropped).

**How to apply:** A cheap, high-coverage automated check beats manual spot-checking for this repo's
translation PRs:
1. Diff frontmatter YAML *keys* (not values) between each translated file and its English source.
2. Diff heading level sequences (`#`/`##`/...) ignoring text, after stripping code fences.
3. Extract fenced code blocks and diff line-by-line ignoring `# ...`/`// ...` trailing-comment
   text — catches corrupted commands/env vars/TOML while tolerating translated comments (this repo's
   translators do translate in-code comments, which is expected, not a defect).
4. Resolve every internal markdown link (`[text](path)`) against the actual file tree per locale.

Running all four checks with a short Python script over every file caught zero false positives and
needed no manual reading of full file bodies — cross-validated against three parallel per-language
subagent reviews (ko/ja/zh-cn) that all independently returned "no structural issues found."
Known accepted limitation in this repo (do not flag): `#fragment` anchors on cross-reference links
still use English-derived heading slugs since translated headings generate different auto-slugs.
