---
title: Admin & Remote Provisioning
description: Enable shunt's admin web surface to provision Claude accounts remotely and inspect account-pool health.
---

shunt can expose an admin-authenticated web surface for provisioning upstream Claude accounts and viewing the health of each `claude_oauth` account pool. It is opt-in: when `[server.admin]` is absent, none of the `/admin*` routes are registered and shunt's default HTTP surface is unchanged.

This builds on the [Anthropic multi-account](/guides/anthropic-multi-account/) store and selection behavior. The browser flow creates one-year, inference-only setup-token accounts; importing a refreshable Claude Code login remains CLI-only.

## Enable the admin surface

Add the optional table and provide at least one admin credential through the configured environment variable:

```toml
[server.admin]                        # all keys optional; defaults shown
header = "x-shunt-admin-token"
tokens_env = "SHUNT_ADMIN_TOKENS"
session_ttl_secs = 3600
pending_ttl_secs = 600
```

```bash
export SHUNT_ADMIN_TOKENS="ops:$(openssl rand -hex 32)"
shunt check
shunt run
```

Credentials use the same comma-separated `name:token` format as `SHUNT_CLIENT_TOKENS`, but they are a separate security boundary. Do not reuse a `[server.auth]` client token as an admin token. Startup fails closed if `[server.admin]` is present but its token environment variable is unset, empty, or malformed.

See the [configuration reference](/reference/configuration/#serveradmin-optional) for every key and default. The [endpoint reference](/reference/endpoints/) lists the browser and JSON routes.

## Provision an account in the browser

1. Open `/admin` and sign in with an admin token.
2. Enter an account name containing only lowercase letters, digits, and hyphens, then select **Start**.
3. Open the displayed authorize URL in another tab. Sign in to the target Claude account and approve access.
4. Copy the resulting `<code>#<state>` value back to the admin page and select **Complete**.
5. shunt stores the account. A provider with an empty `accounts` list picks it up on its next request without a restart. Otherwise, add a name-only entry and reload:

   ```toml
   [[providers.anthropic.accounts]]
   name = "backup"
   ```

A started flow remains valid for `pending_ttl_secs` (10 minutes by default), giving the operator time to open the authorization page and paste the result. The completion response reports whether the account was stored and whether the current provider configuration makes it live.

Account-store changes are discovered per request, so scan-mode providers do not need a restart after an account is added or removed.

## Inspect pool health

The dashboard shows account-store metadata and current health for each provider configured with `auth = "claude_oauth"`. It includes the 5-hour, shared 7-day, and `7d_oi` utilization observed from upstream responses, along with unified status, remaining cooldown, near-quota state, and whether the account is currently available.

The account list exposes only metadata: account name, credential kind (`setup_token` or `imported`), expiry, and UUID. It never returns token material. See [Anthropic Multi-Account](/guides/anthropic-multi-account/#selection-and-proactive-rotation) for how shunt uses quota state, cooldowns, and model-aware weekly buckets when choosing an account.

For API/curl access to account metadata, pool health, or account removal, send the admin token in the configured header (default `x-shunt-admin-token`) and use the JSON routes documented in [HTTP Endpoints](/reference/endpoints/). Header-authenticated requests do not use the browser session and are exempt from CSRF checks; perform setup-token provisioning through the dashboard flow above.

## SSH and refreshable-import fallback

Use the CLI when the shunt host is not reachable in a browser or when you need a refreshable imported login. Over SSH, the long-lived flow prints an authorize URL that you can open on a laptop and accepts the resulting code back in the remote terminal:

```bash
shunt login claude --name backup --long-lived
```

To import the host's current refreshable Claude Code login instead, omit `--long-lived`:

```bash
shunt login claude --name primary
```

The browser admin flow intentionally supports setup-token provisioning only. A refreshable import reads the host's Claude Code credential and therefore stays CLI-only.

## Security

- Put the admin surface behind HTTPS or a trusted tunnel such as WireGuard or Tailscale. shunt serves plain HTTP itself; use TLS termination in front when exposing it remotely.
- Generate a strong admin token and keep it separate from `[server.auth]` client credentials. Admin access can add and remove upstream accounts.
- Browser login creates an HttpOnly, SameSite=Strict session cookie. The cookie is Secure except on loopback hosts, so local HTTP development still works.
- Mutating browser requests require a per-session `x-csrf-token` and pass a same-origin check. API/curl calls authenticate with the admin header instead and do not carry ambient cookie authority.
- Provisioning completion is rate-limited. shunt never logs or returns token material, and account additions and removals are audit-logged by account name.

Without `[server.admin]`, the routes do not exist. This is stronger than leaving an unused dashboard unauthenticated: the admin surface is absent unless explicitly enabled.
