---
name: locale-anchor-parity-drift
description: The chronic doc defect in shunt — translated pages keep English cross-page fragments, and locale reference pages lag the English config sections
metadata:
  type: project
---

The dominant, recurring documentation defect in this repo is **locale drift**, in two distinct
shapes. Expect both on every sweep.

**Shape 1 — dead English fragments on translated pages.** A translator copies the English
`[text](/xx/guides/foo/#english-anchor)` link but Astro derives the id from the *translated*
heading, so the fragment resolves to nothing. Concentrated in the pages that cross-link most:
`guides/effort-and-context`, `guides/codex`, `guides/connect-claude-code`, `guides/shared-gateway`,
`reference/endpoints`, `reference/configuration`. Same offending anchor usually appears in all
three locales at the same line number, because they were translated from a common English revision.

**Shape 2 — locale reference pages missing whole sections.** `reference/configuration.md` had 36
headings in English vs 27–28 per locale; the missing ones cluster on newer subsystems
(`[server.access_control]`, `[server.limits]`, `[server.timeouts]`, `[server.rate_limits]`,
`[server.admin.oidc]`, `[server.gateway.oidc]`, `[server.oauth_usage]`, `[server.status]`).
`reference/cli.mdx` similarly lags on `shunt dashboard setup` and `shunt login antigravity`.

**Why:** AGENTS.md requires English + three locales in the same PR, but a locale section added
later is invisible to reviewers, and anchors are invisible to `cargo test` and CI entirely — the
site is only built on deploy, and `deploy-docs` does not run on PRs.

**How to apply:** diff heading lists between the English page and each locale as a first-class
check (`diff <(grep '^#' en) <(grep '^#' ko)`), and treat a *count* mismatch on
`reference/configuration.md` as the canary for a config key that shipped English-only. When
reporting an anchor fix, take the replacement id from the locale file's own heading — never invent
it. An English fragment is wrong only when the target page has a real locale source file that
lacks the section id; it is correct on a fallback route (no locale source file), and when the
locale page exists but has no counterpart section, AGENTS.md says to link the English page rather
than invent a locale anchor.

Related: [[site-anchor-validation]]
