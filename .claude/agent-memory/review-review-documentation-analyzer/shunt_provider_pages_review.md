---
name: shunt-provider-pages-review
description: PR #230 provider-page review found standalone ordered-upstream defaults and shunt-check credential wording traps
metadata:
  type: project
---

PR #230's per-provider pages introduced two repeatable documentation traps.

**Why:** In ordered `[[upstreams]]` mode, `server.default_provider` still defaults to `anthropic`, but only declared upstream names exist; therefore a page showing only a non-Anthropic upstream fails `shunt check` unless it also sets `server.default_provider` to that upstream (or declares an upstream named `anthropic`). Separately, `shunt check` validates that an `api_key` auth mapping names an env var, but it does not require that env var's value to be exported; the missing value surfaces only when inference resolves the credential.

**How to apply:** For every standalone non-Anthropic `[[upstreams]]` quick start, include a matching `[server] default_provider` or explain that the snippet must be merged into a config containing `anthropic`. Do not promise that `shunt check` catches an unexported provider API key; say the first routed request fails authentication instead.
