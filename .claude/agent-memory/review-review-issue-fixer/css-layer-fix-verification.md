---
name: css-layer-fix-verification
description: Cascade-layer bugs in ui/src/index.css cannot be tested in vitest (jsdom has no layers) — verify by brace-depth in the built dist CSS instead.
metadata:
  type: feedback
---

CSS cascade-layer defects in `ui/src/index.css` must be verified against the **built**
bundle, not a test: `npm run build` in `ui/`, then check the rule's brace depth in
`ui/dist/assets/index-*.css` (depth 0 = unlayered, which is what the `:root` theme-variable
blocks and the `prefers-color-scheme` override need in order to beat `@layer base`).

**Why:** jsdom implements no cascade layers, so any vitest assertion about which rule wins
passes whether the bug is present or not — a vacuous green. A reviewer flagged exactly this
on PR #600 and told the fixer not to add such a test.

**How to apply:** whenever a finding concerns `@layer`, specificity, or theme-variable
precedence in the admin UI stylesheet. Related: [[shunt-mod-typecheck-boundary]].
