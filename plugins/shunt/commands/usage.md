---
description: The shunt gateway pool's remaining headroom and reset times
---

Report the shunt gateway's shared account pool usage.

This session is running without function hooks, so the `shunt` mod could not
answer this command itself. Fall back to reading the endpoint directly:

1. Resolve the gateway base URL from `SHUNT_BASE_URL`, else `ANTHROPIC_BASE_URL`.
   If neither is set, tell the user this session is not pointed at a shunt
   gateway and stop — do not call Anthropic's own API.
2. Resolve the client token from `SHUNT_TOKEN`, else `ANTHROPIC_AUTH_TOKEN`
   (send it as `Authorization: Bearer <token>`), else `ANTHROPIC_API_KEY` (send
   it as `x-api-key: <token>`).
3. `GET <base>/usage` with that header, then report the result.

Do not print the token itself, and do not write it into a file.

Read the response as follows. `pool` is the aggregate across every pooled
provider and `providers` is the same aggregate per configured provider. For
each of the three windows — `5h` (rolling session), `7d` (shared weekly) and
`fable` (the Fable-scoped weekly window) — `remaining` is the fraction of the
pool's combined capacity still **unused**, so `0.62` means 62% of the headroom
is left, not that 62% is spent; `null` means no account reported that window.
`resets_at` is unix epoch seconds, the earliest reset among the accounts
counted. The figures are a shared pool-wide mean, not a prediction that the
next request will be admitted.

A `404` means the gateway is running but `GET /usage` is not enabled: it needs
an `[server.usage]` table in the gateway config, which in turn requires
`[server.auth]`. A `401` or `403` means the gateway refused this session's
client token.

Enable the mod to get this answer instantly and without a tool call:

```
CLAUDE_CODE_ENABLE_FUNCTION_HOOKS=1 claude
```
