---
name: shunt_pr530_admin_state_display_docs_review
description: PR #530 (admin dashboard state display, issues #511-513) doc review — clean pass; worked example note text verified character-for-character against ObservedAccounts.tsx statusNote.
metadata:
  type: project
---

PR #530 fixed three admin-dashboard display bugs split out of #508: near_quota-vs-cooldown
ladder ordering mismatch between `poolState` (PoolHealth.tsx) and `managedState` (accounts.ts)
(#512), a hidden Fable cooldown note when both cooldowns are active (#511, ObservedAccounts.tsx
`statusNote`), and add-account-form interactivity during an open auth flow (#513,
AddClaudeAccount.tsx radios + useProvisioningFlow `start`).

Doc change: one paragraph appended to `docs/m9-admin-surface.md` (not `docs/admin-ui-delivery.md`,
which is a delivery/infra record with no display-logic content — right document confirmed by
grepping it for the state vocabulary, zero hits).

Verified accurate:
- The claimed six-step ladder (`disabled`, `needs_relogin`, `!has_state`, account-wide cooldown,
  `near_quota`, Fable-only cooldown) matches both `poolState` and `managedState` post-fix exactly.
- The worked example `retries in 10m · Fable retries in 30m` reproduces exactly what
  `statusNote` in ObservedAccounts.tsx renders for cooldown_secs_remaining=600 /
  cooldown_fable_secs_remaining=1800 — traced the join logic (`.filter(Boolean).join(' · ')`)
  line by line rather than eyeballing it.
- Pre-existing `cooling-fable` paragraph (~line 431) still true after the fix — it describes the
  Fable-only-active case, untouched by the near_quota/cooldown reorder.
- `site/.../guides/admin-remote-provisioning.mdx` (en/ko/ja/zh-cn) describes neither state
  precedence nor radio interactivity, so needed no update and author's "nothing else" claim held.
- No site/README locale surface touched by this diff — correct, since docs/ is English-only and
  no site page needed a change here.

No findings. This is the second `docs/m9-admin-surface.md`-anchored review after
[[shunt_pr289_tool_search_default_review]]-style "verify the empirical claim, don't eyeball it"
method paid off again: reproducing the exact rendered string beats assuming a code description
"sounds right."
