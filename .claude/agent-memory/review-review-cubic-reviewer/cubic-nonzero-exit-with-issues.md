---
name: cubic-nonzero-exit-with-issues
description: cubic review -j can exit 1 with empty stderr while still printing a valid issues array; verify the finding before trusting it.
metadata:
  type: reference
---

On the feat/863-chatgpt-plan-provider review (PR #871, base origin/main), `cubic review -j -b origin/main` exited with status 1, wrote an empty stderr, and still printed a complete `{"issues": [...], "repository_context": {...}}` document.

**Why:** The agent definition says to stop on non-zero exit, but the JSON was complete and the finding checked out against the code. Stopping would have dropped a real regression (browser-context stripping removed from `passthrough_request_headers`).

**How to apply:** When cubic exits non-zero, check whether stdout parses as the issues JSON. Report the exit status explicitly, mark findings as produced despite the non-zero exit, and verify each finding in the code before reporting it. Do not retry.
