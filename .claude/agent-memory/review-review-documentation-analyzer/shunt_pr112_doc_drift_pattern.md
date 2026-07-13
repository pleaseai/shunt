---
name: shunt-pr112-doc-drift-pattern
description: PR #112 (message_start input_tokens estimate) doc-drift review — recurring pattern of one doc surface updated while sibling spec doc + locale mirrors lag
metadata:
  type: project
---

PR #112 on pleaseai/shunt (branch `amondnet/codex-subagent-msgstart-input`) changed
`message_start`'s `usage.input_tokens` from hardcoded `0` to a local tiktoken estimate for
`responses`-routed providers with `count_tokens = "tiktoken"` (default), via
`AnthropicSseMachine::with_input_estimate` (src/model/responses.rs, src/adapters/responses/mod.rs).

The PR correctly updated `site/src/content/docs/guides/effort-and-context.md` (English) with a new
paragraph on the per-subagent panel reading `message_start`. But it missed two sibling doc
surfaces, repeating a pattern worth checking on every future shunt PR that touches SSE/usage
semantics:

1. **`docs/m1-responses-translation.md`** (the milestone/spec doc, §6 SSE state-machine table,
   ~line 109) still asserts `message_start`'s payload is `{...,usage:{...0}}` unconditionally —
   now false for the default config. AGENTS.md explicitly requires `docs/` milestone/spec files to
   be updated "when implementation behavior deviates from it" in the *same* PR — this is exactly
   that case and was missed. High-confidence finding (≈90, critical/important) each time a
   protocol-shape claim in this spec table goes stale.

2. **Locale mirrors** (`site/src/content/docs/{ko,ja,zh-cn}/guides/effort-and-context.md`) were not
   touched, so they now lack the new paragraph the English source gained. See
   [[shunt_i18n_docs_structure]] — this repo's precedent (PR #52) added all 3 locales in one
   dedicated translation PR rather than syncing every English edit inline, so locale lag by itself
   may be expected workflow rather than a bug. Report it, but at moderate (not critical) confidence
   unless the pre-existing locale text actively contradicts new behavior (it didn't here — it was
   merely silent on the new subagent-panel behavior, not wrong about it).

**How to apply:** for any shunt PR that edits `site/src/content/docs/guides/*.md` (English/root),
always check (a) whether `docs/*.md` milestone specs describe the same protocol/behavior and need a
matching update, and (b) whether the `ko/ja/zh-cn` mirrors of the touched guide diverge — grep for
the specific stale claim (e.g. `usage:{...0}` / `input_tokens.*0`) across `docs/` and all locale
folders rather than assuming a single English edit is complete.
