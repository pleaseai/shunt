---
title: Gateway Login
description: Let Claude Code sign in to shunt with the OAuth device flow and local approval users.
---

Gateway login gives each Claude Code user their own rotating OAuth session instead of distributing one shared client token. It is an opt-in surface: without `[server.gateway]`, none of the OAuth or device-approval routes exist.

:::caution[Choose this only for managed, multi-user access]
A gateway login session has the Claude apps gateway feature trade-offs: WebSearch is disabled, first-party-only beta headers and the one-hour cache-TTL beta are omitted, and sign-in requires a browser. For a personal or single-user setup, keep using [`ANTHROPIC_BASE_URL`](/guides/connect-claude-code/) and optional [`[server.auth]`](/guides/shared-gateway/) instead.
:::

## 1. Configure the login surface

Create a signing secret of at least 32 bytes and a comma-separated list of `email:secret` approval users. Keep both in shunt's environment, not in `shunt.toml`:

```bash
export SHUNT_GATEWAY_JWT_SECRET="$(openssl rand -base64 48)"
export SHUNT_GATEWAY_USERS='alice@example.com:<unique-secret>,bob@example.com:<unique-secret>'
```

Add the public URL that Claude Code and users' browsers can reach:

```toml
[server.gateway]
public_url = "https://gateway.example.com"
jwt_secret_env = "SHUNT_GATEWAY_JWT_SECRET" # default
users_env = "SHUNT_GATEWAY_USERS"            # default
token_ttl_seconds = 3600                      # default
trust_forwarded_for = false                   # default
```

Startup fails closed if `public_url` is not a bare HTTPS origin (`http` is allowed only on loopback), `token_ttl_seconds` is zero, the signing secret is shorter than 32 bytes, or the user list is empty or malformed. A secret may contain `:` because only the first colon separates an email from its secret.

Use HTTPS for every non-loopback deployment. By default, `/device` ignores `X-Forwarded-For` and `X-Real-IP` and rate-limits the socket peer. If shunt is reachable exclusively through a trusted reverse proxy, set `trust_forwarded_for = true` and configure that proxy to remove client-provided forwarding headers before setting its own trusted client address. Never enable this option on a directly exposed gateway.

## 2. Push managed Claude Code login settings

Set these [managed settings](https://code.claude.com/docs/en/settings) on each developer machine:

```json
{
  "forceLoginMethod": "gateway",
  "forceLoginGatewayUrl": "https://gateway.example.com"
}
```

Managed settings locations depend on the platform:

- macOS: `/Library/Application Support/ClaudeCode/managed-settings.json`
- Linux and Windows (WSL): `/etc/claude-code/managed-settings.json`
- Windows native: `C:\Program Files\ClaudeCode\managed-settings.json`

The URL must equal `public_url`. Claude Code reads the OAuth endpoint paths from shunt's discovery document. The issued bearer gates `/v1/models` and inference requests whose selected provider injects a server-side credential; passthrough providers remain open.

## 3. Sign in

Start Claude Code and run `/login`. The CLI shows a device code and opens the gateway's `/device` page. On that page:

1. Confirm the displayed device code.
2. Enter an email and secret from `SHUNT_GATEWAY_USERS`.
3. Select **Approve device**.
4. Return to Claude Code after the success page appears.

Pre-filling the code never auto-approves it. The approval POST is same-origin protected; a cross-site submission is blocked with a notice instead of changing the grant.

## Managed settings and model policy

After sign-in, shunt serves the user's resolved policy from authenticated
`GET /managed/settings`. Configure ordered `[[server.gateway.policies]]` entries:

```toml
[[server.gateway.policies]]
[server.gateway.policies.match]
emails = ["alice@example.com"]
[server.gateway.policies.cli]
availableModels = ["claude-opus-4-8"]
[server.gateway.policies.cli.env]
DISABLE_UPDATES = "1"

[[server.gateway.policies]]
match = {} # catch-all
[server.gateway.policies.cli.permissions]
deny = ["WebFetch"]
```

All catch-all entries merge in order. The first email-specific match then merges
on top. Objects merge recursively, allow-list arrays replace, and arrays whose
key contains `deny` are unioned without duplicates. A configured policy always
returns `200`, even when it resolves to `{}`; omitting `policies` returns `404`
so Claude Code can distinguish “no managed policy.” Responses include a stable
per-user `uuid`, a settings `checksum`, and the same checksum as `ETag`;
`If-None-Match` returns `304` when unchanged.

When `availableModels` resolves to an array of strings, shunt also enforces it on
`/v1/messages` and `/v1/messages/count_tokens` for that gateway user. A denied
model receives `400 invalid_request_error` without contacting the upstream.

A non-empty telemetry destination list pushes the six standard Claude Code OTLP
environment values. Policy `env` keys override injected defaults:

```toml
[server.gateway.telemetry]
[[server.gateway.telemetry.forward_to]]
url = "https://collector.example.com"
# headers = { "x-api-key" = "..." }
```

This configuration gates the managed environment push now. The authenticated
OTLP ingest/relay routes arrive separately in M-C (#189).

## Session behavior

Access tokens are HS256 JWTs with a one-hour default lifetime. Claude Code silently refreshes them. Every refresh rotates the opaque refresh token; replaying a retained old token within the 30-day, 64-tombstone bound invalidates the active token in that rotation family and makes Claude Code sign in again.

Device grants, refresh tokens, and attempt counters are in memory in this milestone. Config hot reload keeps them, and changes to the signing secret or user list hot-apply. Expired grants and idle rate-limit entries are removed opportunistically; device grants and rate-limit identities are each capped at 4,096 entries. Used refresh-token tombstones are retained for 30 days and capped at 64 per family. A shunt process restart clears device grants and refresh sessions; existing access JWTs remain valid until expiry, after which users must sign in again. Adding or removing the `[server.gateway]` table itself requires a restart because route registration is fixed at boot.

When [`[server.auth]`](/guides/shared-gateway/) and `[server.gateway]` are both configured, they compose: either a valid static client token or a valid gateway bearer grants access. This supports a staged migration without breaking existing clients.

## What comes next

Managed policy, `ETag` caching, telemetry environment push, and server-side model
allow-list enforcement are described above. Authenticated inbound OTLP telemetry
remains the separate M-C follow-up.
