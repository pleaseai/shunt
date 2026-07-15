---
name: shunt-account-scan-cache-comment-rot
description: Comment-rot checks for the mtime-keyed Claude/Codex account-store scan cache.
metadata:
  type: project
---

The account-store scan cache is a documentation hotspot: distinguish a per-request directory metadata check from an actual `read_dir` scan, and scope “zero credential-file reads” to discovery because selected-account authentication still reads its credential file.

**Why:** The cache is keyed by `(provider label, lexical store directory path)`, so Claude and Codex remain separate even when overrides point to the same path. Comments saying `provider_label` affects error text only are therefore stale. Concurrent cache misses can also observe different snapshots if a store mutation overlaps their scans, so equal pre-scan mtimes do not guarantee identical results.

**How to apply:** Whenever this cache or account-store write paths change, re-check comments about per-request scans, total I/O, provider-label use, cross-store key separation, concurrent misses, and categorical mtime invalidation. Internal Claude writes use same-directory temp-file creation plus rename and removals use `remove_file`, but equal observed mtimes remain possible on coarse filesystems and a directory can change after a request samples its metadata.
