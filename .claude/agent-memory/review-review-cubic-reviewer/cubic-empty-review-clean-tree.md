---
name: cubic-empty-review-clean-tree
description: cubic review -j with no -b on a clean working tree silently reviews nothing (false-clean result) — always pass -b explicitly when reviewing a committed branch/PR
metadata:
  type: feedback
---

`cubic review -j` (no `-b`/`-c`) reviews **uncommitted changes only**. On a clean working
tree (already-committed PR/branch, e.g. issue #310's `2d67f01`), this silently produces
`{"issues": []}` — a false "clean review", not an error. cubic gave no warning that it saw
an empty diff.

**Why:** the task's "user-provided flags" were just `-j` (JSON-mode preference), not an
explicit instruction to skip base-branch comparison. Blindly following "user provided flags
→ use exactly those" produced a meaningless review of a repo with no uncommitted changes.

**How to apply:** when the working tree is clean and the task is to review a specific
committed branch/commit against a base (e.g. "branch vs origin/main"), always add
`-b <base-branch>` (or `-c <commit>`) even if the caller only mentioned `-j`. Treat a
plugin-provided "user flags" string that omits `-b`/`-c` as ambiguous, not authoritative,
when the working tree is clean — verify with `git status --porcelain` first. Re-running with
`cubic review -j -b origin/main` on this repo took ~2+ minutes; ran in background via
`run_in_background` and polled with a `ps`-based wait loop rather than blocking `sleep`.
