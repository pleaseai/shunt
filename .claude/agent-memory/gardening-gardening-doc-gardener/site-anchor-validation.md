---
name: site-anchor-validation
description: How to validate shunt site/ links and anchors without false positives — route resolution, English-fallback pages, and the exact github-slugger rules that bite
metadata:
  type: project
---

Validating links in `site/src/content/docs/` requires three project-specific rules. A naive
filesystem-path checker reports ~60% false positives here.

**Why:** the site is Astro/Nimbus, so links are *site routes* (`/ko/guides/codex/`), not relative
paths, and `site/src/pages/[...slug].astro` synthesizes an **English fallback page for every
English page that lacks a translation** (its `fallbackPaths` is built by iterating the English
entries). A locale route whose English counterpart exists still resolves without a locale source
file and carries the *English* heading ids; a locale route with **neither** a locale nor an English
source is unresolved.

**How to apply:**

1. Resolve `/<locale>/<path>/` against `site/src/content/docs/<locale>/<path>.{md,mdx}` and
   `.../index.{md,mdx}`. Never treat a leading `/` as a filesystem path.
2. A locale route missing its source file is **not** a broken link **when the English page
   exists** — it renders the English fallback, so an English fragment on it is *correct*. Resolve
   the fallback the way `fallbackPaths` does (locale route → English entry); if no English entry
   exists either, report the route as unresolved. Only flag anchors on locale pages that have a
   real translation file.
3. Reproduce github-slugger exactly, or you will generate noise:
   - Strip markdown links **before** stripping backticks. `` `[server.gateway.session]`(可选) ``
     is a code span followed by literal text, but strip backticks first and the regex sees a
     markdown link and eats the section name.
   - Replace each space with one dash — do **not** collapse runs. `## Context / usage display for mapped models`
     → `context--usage-display-for-mapped-models` (double dash), because the removed `/` leaves two spaces.
   - Do not strip `<...>` as HTML. `[providers.<name>]` → `providersname`, not `providers`.
   - CJK survives slugging; only punctuation is removed.
4. Site pages are never orphans by link-graph: navigation lives in `site/src/lib/i18n.ts`.
   Check membership there, not inbound links.

Related: [[locale-anchor-parity-drift]]
