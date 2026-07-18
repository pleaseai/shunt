---
title: Gateway Login
description: Let Claude Code sign in to shunt with the OAuth device flow, local approval users, or an OIDC provider such as Google.
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
# state_path = "~/.shunt/gateway-sessions.json"  # default; "" = memory-only sessions
```

Startup fails closed if `public_url` is not a bare HTTPS origin (`http` is allowed only on loopback), `token_ttl_seconds` is zero, the signing secret is shorter than 32 bytes, or neither a valid user list nor a valid external IdP is configured. A static-user secret may contain `:` because only the first colon separates an email from its secret.

### Use Google OIDC instead

Create an OAuth web client in Google Cloud with this exact authorized redirect URI:

```text
https://gateway.example.com/device/callback
```

Put its secret in the gateway environment, then configure the issuer and a mandatory allowlist:

```bash
export SHUNT_GATEWAY_OIDC_SECRET='<google-client-secret>'
```

```toml
[server.gateway.oidc]
issuer = "https://accounts.google.com"
client_id = "<google-client-id>"
client_secret_env = "SHUNT_GATEWAY_OIDC_SECRET" # default
allowed_domains = ["example.com"]
# allowed_emails = ["contractor@outside.example"]
```

Google uses the default `openid email profile` scopes. shunt requires Google UserInfo to return `email_verified = true`, then admits the user only when the case-insensitive full email or domain matches the allowlist.

For GitHub, SAML, or another provider that does not expose the standard OIDC
surface shunt expects, put an OIDC identity provider such as [Dex](https://dexidp.io/docs/connectors/)
in front of it and configure the Dex issuer here. Direct provider-specific OAuth2
integrations are out of scope.

At least one non-empty `allowed_domains` or `allowed_emails` entry is required;
shunt refuses to start without it. `users_env` becomes optional when
`[server.gateway.oidc]` is configured. Leave `SHUNT_GATEWAY_USERS` set to show
both SSO and password sign-in, or unset it to show only the provider button.

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
2. Select the SSO button (**Sign in with Google** for Google, **Sign in with SSO** for other providers), then finish the provider login. If static users are also configured, entering an email and secret and selecting **Approve device** remains available.
3. Return to Claude Code after the success page appears.

Pre-filling the code never auto-approves it. The password approval POST is same-origin protected. The external callback instead binds the cross-site redirect with a single-use, ten-minute OAuth state and PKCE; provider errors are never echoed into the page.

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
returns `200`; when no user-specific or catch-all settings apply, the response
contains only the injected telemetry `env` if telemetry is enabled, and `{}`
otherwise. Omitting `policies` returns `404` so Claude Code can distinguish “no
managed policy.” Responses include a stable
per-user `uuid`, a settings `checksum`, and an RFC-quoted `ETag` containing that
checksum; `If-None-Match` returns `304` when unchanged and also accepts weak,
comma-list, wildcard, and legacy-unquoted validators.

When `availableModels` resolves to an array of strings, shunt also enforces it on
`/v1/messages` and `/v1/messages/count_tokens` for that gateway user. It strips
one trailing Claude Code context-window hint (`[1m]` or `[1M]`) from the
client-requested model before comparison, so `allowed[1m]` matches an `allowed`
entry. A denied model receives `400 invalid_request_error` without contacting
the upstream.

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

Device grants and attempt counters live in memory. Refresh-token sessions survive config hot reload and are persisted by default as described below. Changes to the signing secret, user list, and OIDC configuration hot-apply. Expired grants and idle rate-limit entries are removed opportunistically; device grants and rate-limit identities are each capped at 4,096 entries. Used refresh-token tombstones are retained for 30 days and capped at 64 per family, and an active session that goes 30 days without refreshing expires. Adding or removing the `[server.gateway]` table itself requires a restart because route registration is fixed at boot.

Refresh sessions survive a shunt restart by default: shunt writes the refresh-token store to `state_path` (default `~/.shunt/gateway-sessions.json`, atomically, owner-only permissions (0600 on Unix)) after every grant or rotation and restores it at boot, so users keep refreshing instead of re-running the browser flow. Refresh tokens are stored as SHA-256 hashes — the file never contains a usable credential, only token hashes and the signed-in identities. A missing or corrupt file just falls back to memory-only behavior, as does an environment with no resolvable home directory. Set `state_path = ""` for memory-only sessions, where a restart clears refresh sessions and users sign in again once their access JWT expires. Device grants stay memory-only either way (a restart mid-login only costs that attempt), and the state file must not be shared between concurrently running shunt processes.

Note that refresh grants mint tokens from the identity stored with the session and do not re-check the static user list or external IdP allowlist, so removing a user from either approval source does not end an existing session. To deprovision a user immediately, also delete the state file (or set `state_path = ""`) and restart.

When [`[server.auth]`](/guides/shared-gateway/) and `[server.gateway]` are both configured, they compose: either a valid static client token or a valid gateway bearer grants access. This supports a staged migration without breaking existing clients.

## What comes next

Managed policy, `ETag` caching, telemetry environment push, and server-side model
allow-list enforcement are described above. Authenticated inbound OTLP telemetry
remains the separate M-C follow-up.
