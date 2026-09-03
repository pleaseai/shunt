---
name: workspace-mutates-during-review
description: ocr preview can flip from range to workspace mid-session when the lead session (or another agent) writes to the same worktree concurrently
metadata:
  type: feedback
---

During an agy-429 review, `git status --porcelain` was clean at session start
(matching the stored gitStatus snapshot), so the review proceeded in range
mode (`--from origin/main --to HEAD`). Partway through, the lead session that
dispatched this review kept applying review fixes to the same three files in
the working tree (the sibling `review-cubic-reviewer` ran finder-only and
edited nothing), producing a genuine uncommitted diff against HEAD that
exactly matched the task's literal "review the current uncommitted workspace
changes" wording.

**Why:** `Read`/`git diff` always reflect live disk state, not a frozen
snapshot, and the shared worktree means the lead — or any agent running in
parallel (ensemble-review dispatches several finders at once) — can land
edits mid-run.
The initial clean `git status` is not a guarantee of a clean status ten tool
calls later.

**How to apply:** when the task's phrasing insists on "current uncommitted
changes" but the initial `git status` looked clean, re-check `git status
--porcelain` and `ocr delegate preview` (no `--from/--to`) again before
finalizing scope — don't trust only the first snapshot. If a real workspace
diff appears, prefer it over the range mode chosen at session start, since it
is what the caller actually meant. See
[[catalog-fetch-slots-sibling-map-eviction]] for the concrete finding this
surfaced.
