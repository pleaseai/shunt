---
name: token-file-writers
description: Two credential-file atomic writers exist with different permission-hardening; one has a world-readable window.
metadata:
  type: project
---

shunt has two atomic credential-file writers with DIFFERENT hardening:

- `src/auth/codex_auth.rs::write_auth_file_atomic` → `write_private`: born-private via `OpenOptions::create_new().mode(0o600)` (temp file created 0600 from the start). Explicit comment: chmod-after-write leaves a umask-default window on multi-user hosts. Used by `claude_store.rs` (initial store-account writes) and codex.
- `src/auth/claude_auth.rs::write_atomic` (line ~241): `fs::write(temp, ...)` THEN `set_private_permissions(temp)` — chmod-AFTER-write, so the temp file transiently sits at umask default (often 0644) before the chmod. Used by `ClaudeAuthStore` for every token refresh / write-back (credentials-file accounts AND store-managed refreshes, including the new `force_refresh`).

**Why:** PR #70 (Anthropic multi-account) exercises `write_atomic` far more (every pooled account's refresh). The born-private sibling shows the authors know the window matters, but claude_auth's writer wasn't unified.
**How to apply:** If asked to harden credential storage, unify claude_auth's `write_atomic` onto the born-private pattern. Severity is low on single-user/loopback deployments (shunt's norm), higher on shared multi-user hosts.

Verified-SAFE in the same area (do not re-flag): `[[providers.*.accounts]].name` is `[a-z0-9-]+`-validated at every entry point → no path traversal in `account_path`; `host_is_loopback`/`host_is_anthropic` carve-out only matches literal localhost/loopback IPs (no DNS-rebind or userinfo bypass — url crate parses authority); per-account outbound headers are rebuilt from the original client headers each loop iteration → no cross-account bearer leak; `claude_login.rs` reads the setup-token via `rpassword::prompt_password` and never prints it. See [[project_sentry-pii-egress]] for the separate log/breadcrumb egress concern.
