---
name: local-greptile-ada-url-crash
description: Local greptile CLI on this machine crashes at dyld load time (node/ada-url mismatch) before running any review — both --json and --agent fail identically
metadata:
  type: project
---

On this machine (/Users/lms/orca/workspaces/shunt/dashboard and presumably other worktrees of the same
repo), `greptile review --json` and `greptile review --agent` both fail with the same dyld error before
any CLI logic runs:

```
dyld[PID]: Library not loaded: /usr/local/opt/ada-url/lib/libada.3.dylib
  Referenced from: <...> /usr/local/Cellar/node/26.4.0/bin/node
  Reason: tried: ... (no such file) ...
```

Exit code 134 (SIGABRT). This happens even before `greptile whoami` can be meaningfully checked — it is a
Homebrew Node.js / ada-url native dependency version mismatch, not a Greptile auth or flag problem.

**Why:** `ada-url` is a native addon dependency of a Node package greptile's CLI depends on
(likely via `undici`/`fetch` URL parsing). A Homebrew upgrade of `node` or `ada-url` left the
versions out of sync (dylib expects `ada-url@3` but only `4.0.0` cellar exists), so the Node
binary itself aborts at dynamic-link time.

**How to apply:** When invoking the Greptile CLI in review workflows on this machine, expect this crash.
Do not attempt an install/fix yourself (e.g. `brew reinstall ada-url node`) unless explicitly asked —
report `skipped-unavailable` with the exact dyld error text and stop; do not substitute your own
freeform diff review as a replacement for the structured Greptile findings, since callers depend on the
`{"findings": [...]}` schema being either present or explicitly absent (not silently backfilled).
See also the broader note in the user's global auto-memory index: `review-engine-env-traps.md`
("cubic 기본 모드는 미추적 `.claude/agent-memory/` 때문에 죽는다 ... 로컬 greptile은 node/ada-url
불일치로 불가").
