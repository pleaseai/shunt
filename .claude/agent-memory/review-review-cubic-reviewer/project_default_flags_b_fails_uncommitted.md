---
name: project-default-flags-b-fails-uncommitted
description: shunt's configured REVIEW_CUBIC_DEFAULT_FLAGS (--json -b) returns Bad Request when run against uncommitted local changes; fall back to plain -j.
metadata:
  type: project
---

In the shunt repo (pleaseai/shunt), `.please/config.yml` sets `REVIEW_CUBIC_DEFAULT_FLAGS` to `--json -b`. Running `cubic review --json -b` against a branch with uncommitted working-tree changes (e.g. `amondnet/97` with modified `src/model/responses.rs`) returned:

```json
{ "issues": [], "error": "Bad Request" }
```

with exit code 1. Likely cause: `-b` expects to diff against a pushed/comparable base (e.g. remote branch state), which doesn't exist cleanly when there are uncommitted local edits.

**How to apply:** If the default-flags command exits non-zero with an `"error"` field in the JSON body (not just non-JSON failure), retry with plain `cubic review -j` (no `-b`) before reporting a hard failure to the user. This matched rule 3 in the skill instructions (no flags + uncommitted changes → `cubic review -j`) better than the configured default anyway.
