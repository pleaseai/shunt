---
name: shunt-account-scan-cache-comment-rot
description: Comment-rot checks for the mtime-keyed Claude/Codex account-store scan cache.
metadata:
  type: project
---

The account-store scan cache is a documentation hotspot: distinguish a per-request directory metadata check from an actual `read_dir` scan, and scope “zero credential-file reads” to discovery because selected-account authentication still reads its credential file.

**Why:** The cache is keyed only by the lexical store directory path, while Claude and Codex directory environment overrides can point to the same path; comments must not claim the keys can never collide. Concurrent cache misses can also observe different snapshots if a store mutation overlaps their scans, so they are not guaranteed to store identical results.

**How to apply:** Whenever this cache or account-store write paths change, re-check comments about per-request scans, total I/O, cross-store key separation, concurrent misses, and categorical mtime invalidation. Internal Claude writes use same-directory temp-file creation plus rename and removals use `remove_file`, but equal observed mtimes remain possible on coarse filesystems.
