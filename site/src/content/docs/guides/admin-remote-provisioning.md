---
title: Admin & Remote Provisioning
description: Enable shunt's admin web surface to provision Claude accounts remotely and inspect account-pool health.
---

shunt can expose an admin-authenticated web surface for provisioning upstream Claude accounts and viewing the health of each `claude_oauth` account pool. It is opt-in: when `[server.admin]` is absent, none of the `/admin*` routes are registered and shunt's default HTTP surface is unchanged.

This builds on the [Anthropic multi-account](/guides/anthropic-multi-account/) store and selection behavior. The browser form can create a refreshable full-OAuth account or a one-year, inference-only setup-token account. Importing an existing Claude Code credential file remains CLI-only.

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
2. Enter an account name containing only lowercase letters, digits, and hyphens.
3. Select **Full OAuth (refreshable)** (the dashboard default) or **Setup token (1-year, inference-only)**, then select **Start**.
4. Open the displayed authorize URL in another tab. Sign in to the target Claude account and approve access.
5. Copy the resulting `<code>#<state>` value back to the admin page and select **Complete**.
6. shunt stores the account. A provider with an empty `accounts` list picks it up on its next request without a restart. Otherwise, add a name-only entry and reload:

   ```toml
   [[providers.anthropic.accounts]]
   name = "backup"
   ```

A started flow remains valid for `pending_ttl_secs` (10 minutes by default), giving the operator time to open the authorization page and paste the result. The server records the selected mode with the pending attempt, so the completion request cannot switch token types. Full OAuth stores access and refresh tokens and appears as credential kind `imported`; setup-token mode stores a static credential with kind `setup_token`. The completion response reports whether the account was stored and whether the current provider configuration makes it live.

Account-store changes are discovered per request, so scan-mode providers do not need a restart after an account is added or removed.

## Inspect pool health

The dashboard shows account-store metadata and current health for each provider configured with `auth = "claude_oauth"`. It includes the 5-hour, shared 7-day, and `7d_oi` utilization observed from upstream responses, along with unified status, remaining cooldown, near-quota state, and whether the account is currently available.

The account list exposes only metadata: account name, credential kind (`setup_token` or `imported`), expiry, and UUID. It never returns token material. See [Anthropic Multi-Account](/guides/anthropic-multi-account/#selection-and-proactive-rotation) for how shunt uses quota state, cooldowns, and model-aware weekly buckets when choosing an account.

For API/curl access to account metadata, pool health, provisioning, or account removal, send the admin token in the configured header (default `x-shunt-admin-token`) and use the JSON routes documented in [HTTP Endpoints](/reference/endpoints/). Header-authenticated requests do not use the browser session and are exempt from CSRF checks. Start provisioning with `{ "name": "backup", "mode": "oauth" }` or `mode: "setup_token"`; omitting `mode` keeps the API's backward-compatible `setup_token` default.

## CLI and SSH fallback

Use the CLI when the shunt host is not reachable in a browser. Full OAuth normally opens a browser and completes through a temporary `127.0.0.1` callback; over SSH or in a headless environment, force the same manual-paste redirect used by the admin page:

```bash
shunt login claude --name backup --mode oauth --manual
```

To import the host's current refreshable Claude Code login instead:

```bash
shunt login claude --name primary --mode import
```

To create a one-year inference-only credential:

```bash
shunt login claude --name ci --mode setup-token
```

`--long-lived` remains a deprecated alias for `--mode setup-token`. The admin surface supports full OAuth and setup-token provisioning; only import requires access to the host's existing Claude Code credential and therefore stays CLI-only.

:::caution[Refresh-token rotation]
A refreshable account must have one active owner. OAuth refresh may replace the refresh token and invalidate an older copy, so do not share one store file across processes or copy it to another independently running host. Provision each process separately, or choose setup-token mode where a static non-refreshable credential is appropriate.
:::

## Security

- Put the admin surface behind HTTPS or a trusted tunnel such as WireGuard or Tailscale. shunt serves plain HTTP itself; use TLS termination in front when exposing it remotely.
- Generate a strong admin token and keep it separate from `[server.auth]` client credentials. Admin access can add and remove upstream accounts.
- Browser login creates an HttpOnly, SameSite=Strict session cookie. The cookie is Secure except on loopback hosts, so local HTTP development still works.
- Mutating browser requests require a per-session `x-csrf-token` and pass a same-origin check. API/curl calls authenticate with the admin header instead and do not carry ambient cookie authority.
- Provisioning completion is rate-limited. shunt never logs or returns token material, and account additions and removals are audit-logged by account name.

Without `[server.admin]`, the routes do not exist. This is stronger than leaving an unused dashboard unauthenticated: the admin surface is absent unless explicitly enabled.
