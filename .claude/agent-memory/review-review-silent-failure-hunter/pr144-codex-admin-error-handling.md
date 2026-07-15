---
name: pr144-codex-admin-error-handling
description: PR #144 Codex admin OAuth logs exchange/persistence/list failures correctly; metadata structural omissions and token-URL fallback still hide misconfiguration.
metadata:
  type: project
---

PR #144 (`feat(admin): add Codex account provisioning + pool view`) correctly handles all `spawn_blocking` result arms and logs generic-browser token-exchange/persistence failures without storing partial credentials. Two silent paths remain: `account_meta` uses `value.get("tokens")?`, so a structurally invalid but valid-JSON account disappears without the warnings used for read/JSON errors, and `resolve_oauth_token_url` silently falls back to the production OpenAI endpoint for malformed/unsafe `SHUNT_CODEX_TOKEN_URL` overrides.

**Why:** The first makes the dashboard report fewer/no accounts while the pool still discovers the file; the second can turn a deliberate mock/custom override into an unexpected real OAuth exchange.

**How to apply:** Review future Codex admin/store changes for explicit `Result` propagation or warnings on structural metadata failures, and log rejected endpoint overrides without exposing raw URL/userinfo.
