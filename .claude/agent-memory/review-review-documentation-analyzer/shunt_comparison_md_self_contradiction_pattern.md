---
name: shunt-comparison-md-self-contradiction-pattern
description: docs/comparison.md (+ its 4 site mirrors) is a living gap-analysis doc updated per-issue; §7's one-line takeaway is the part most likely to drift out of sync with §6 item status when only one item is resolved
metadata:
  type: project
---

`docs/comparison.md` (mirrored verbatim in structure, translated in prose, at
`site/src/content/docs/getting-started/comparison.md` + `ko/`/`ja/`/`zh-cn/`) tracks a
numbered list of gap-analysis items in §6 (each tagged `already tracked: [#N]` or
`resolved: [#N]`), summarized in a §7 "one-line takeaway" paragraph that name-drops the
same issue numbers.

**Found in PR #111 (branch amondnet/46, 2026-07-14):** the PR resolved only item C
(#46 — verified accurate against `src/adapters/responses/mod.rs`'s
`open_ws_turn`/`commit_or_fallback`, single commit, only that file touched). Note
"mid-stream WS failure fallback" is loose shorthand: #46 only extends the fallback
to failures *before* the first event (send→first-event window); a transport drop
*after* the first event is intentionally surfaced as an error rather than
restarted — it is not a mid-turn resume. But the §7 rewrite went further than the diff justified: it
changed "hardening the Codex WS continuation (live-probe + mid-stream fallback)"
(future work, both open) to "has since been hardened on both fronts (continuation
live-probe [#45] and pre-first-token HTTP fallback [#46])" — falsely implying #45 (the
separate live-probe-the-continuation-normalization item) was also resolved. Item B in
the same §6, two paragraphs above, was left unchanged and still correctly says
"not yet live-probed" / "already tracked: [#45]". Direct self-contradiction, identically
reproduced across all 5 files (root + 4 translations) since the translations are
structurally mirrored edits of the same paragraph.

**Why this happens:** §7 is prose, not a checklist — it's easy to over-generalize "we
hardened the WS transport" into "both open WS items are hardened" when only one
resolved. The word "probe" is also overloaded in this doc: item C's own text uses
"reuse liveness probe" (#93, unrelated) right next to discussing #46, which may have
bled into conflating it with item B's different "continuation-normalization live
probe" (#45).

**How to apply:** When reviewing any future PR that touches `docs/comparison.md` item
status (marking something "resolved: [#N]"), always re-read the §7 takeaway paragraph
line-by-line against every §6 item's stated status (not just the one being resolved) —
grep for every `[#N]` issue number used in §7 and confirm each one's §6 item still
says the same completion state. Also diff the same paragraph across all 4 site
mirrors — an inconsistency introduced in one is mechanically copied to all four in
this repo's edit style (see [[shunt-i18n-docs-structure]]).
