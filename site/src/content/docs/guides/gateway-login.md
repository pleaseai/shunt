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
```

Startup fails closed if `public_url` is not an HTTP(S) URL, the signing secret is shorter than 32 bytes, or the user list is empty or malformed. A secret may contain `:` because only the first colon separates an email from its secret.

Use HTTPS for every non-loopback deployment. If shunt is behind a reverse proxy, have the proxy replace untrusted `X-Forwarded-For` values with its trusted client address; `/device` uses that address for its per-IP attempt limit.

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

The URL must equal `public_url`. Claude Code reads the OAuth endpoint paths from shunt's discovery document and sends the issued bearer to `/v1/messages`, `/v1/messages/count_tokens`, and `/v1/models`.

## 3. Sign in

Start Claude Code and run `/login`. The CLI shows a device code and opens the gateway's `/device` page. On that page:

1. Confirm the displayed device code.
2. Enter an email and secret from `SHUNT_GATEWAY_USERS`.
3. Select **Approve device**.
4. Return to Claude Code after the success page appears.

Pre-filling the code never auto-approves it. The approval POST is same-origin protected; a cross-site submission is blocked with a notice instead of changing the grant.

## Session behavior

Access tokens are HS256 JWTs with a one-hour default lifetime. Claude Code silently refreshes them. Every refresh rotates the opaque refresh token; replaying an old token invalidates the active token in that rotation family and makes Claude Code sign in again.

Device grants, refresh tokens, and attempt counters are in memory in this milestone. Config hot reload keeps them, and changes to the signing secret or user list hot-apply. A shunt process restart clears them, so all gateway users must sign in again. Adding or removing the `[server.gateway]` table itself requires a restart because route registration is fixed at boot.

When [`[server.auth]`](/guides/shared-gateway/) and `[server.gateway]` are both configured, they compose: either a valid static client token or a valid gateway bearer grants access. This supports a staged migration without breaking existing clients.

## What comes next

This milestone implements login and bearer validation only. Per-user managed policy from `GET /managed/settings` and inbound OTLP telemetry are separate follow-ups. Until the managed-settings endpoint lands, deploy the `forceLoginMethod` and `forceLoginGatewayUrl` settings through your existing device-management system.
