---
name: shunt-config-reload-doc-review-traps
description: Review traps for shunt reload matrices, environment-backed credentials, and dynamically scanned account stores.
metadata:
  type: project
---

When reviewing shunt configuration or lifecycle documentation, verify reload claims against all boot-time wiring rather than relying only on the concise config-reload guide.

**Why:** Environment-backed values have different resolution times (config auth/OIDC credentials during load/reload, API keys and inline account token envs during request resolution), dynamic account-store additions/removals are discovered on the next request without a config reload, and the restart-only surface includes boot-registered route tables and telemetry initialization beyond `server.bind` and `[sentry]`.

**How to apply:** Cross-check reload matrices against `src/reload.rs`, router registration/background-task startup, and credential resolution sites; distinguish a child process's fixed inherited environment from when shunt reads that environment.
