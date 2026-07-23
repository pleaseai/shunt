# 2026-07-22 — Auto-linked dashboard observation

## Decision

Separate read-only usage observation from managed pool provisioning.

The primary dashboard automatically reflects Claude Code and Codex CLI credentials already present on the gateway host. It never refreshes, writes, or copies those source credentials. Managed pool accounts remain explicit shunt-owned credential copies for load-balancing and appear only under an advanced disclosure.

## Why

The earlier UI made account-store filenames (`demo`, `pool-b`) the primary identity and required manual provisioning merely to view usage. That was disconnected from the credentials the running shunt already used. Directly sharing refresh ownership would be unsafe because providers rotate refresh tokens, so observation borrows only a current access token and treats expiry as an unavailable state.

## Implementation boundaries

- Claude Code: configured/default credential file, with macOS Keychain fallback; quota API reads cached for 60 seconds and scoped by access-token hash.
- Codex CLI: configured/default `auth.json`; identity from token claims; quota remains response-derived and is captured for unpooled traffic on both protocol paths.
- Gemini CLI: Code Assist tier plus every model bucket returned by `retrieveUserQuota`.
- Kimi Code: weekly and 5-hour quota from its read-only usage endpoint.
- Grok CLI: first-party billing/product percentages and subscription identity from the CLI OAuth surface.
- Cursor.app: SQLite state opened read-only; an in-memory first-party web session reads usage summary and account identity.
- API responses contain masked identity and usage only, never token material.

## Links

- Issue: https://github.com/pleaseai/shunt/issues/232
- Plan: `docs/plans/dashboard-auto-link/TICKETS.md`
