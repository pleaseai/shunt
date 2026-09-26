---
title: Pool Account Controls
description: Pause an individual pool account and rank available accounts by soonest quota reset — both from the admin dashboard, at runtime, with no config edit or restart.
---

Two runtime controls sit on top of account-pool selection ([Anthropic Multi-Account](/guides/anthropic-multi-account/), [Codex Multi-Account](/guides/codex-multi-account/)): pausing a single account, and ranking available accounts by soonest quota reset instead of burn-rate headroom. Both are operated from the admin dashboard's "Managed pool health" table, or directly against the admin API. Both are memory-only — a restart clears them — so neither touches `shunt.toml`.

## Pausing an account

Pausing excludes one account from selection exactly as its config-side `disabled = true` would, without editing `shunt.toml` or signing the account out. The credential and its quota history are untouched; a paused account simply never appears as a selection candidate until resumed.

This is different from `disabled`:

| | `disabled` (config) | `paused` (runtime) |
| :-- | :-- | :-- |
| Set via | `shunt.toml`, reloaded | Admin dashboard or `PATCH /admin/api/pool/{provider}/accounts/{account_ref}` |
| Survives a restart | Yes | No |
| Use case | Permanently remove an account from the deployment | Temporary operator intervention — pull an account aside for a few minutes without a config round-trip |

From the dashboard: open **Manage pool accounts → Managed pool health**, and click **Pause** on the account's row. Its state shows as `paused`; click **Resume** to bring it back. Both buttons require a write-tier admin session.

Directly against the API (write-tier credential required):

The `account_ref` is the opaque identifier returned on each account object by `GET /admin/api/pool`. Use it rather than the display `name`, so distinct accounts that share a name remain independently addressable.

```bash
curl -X PATCH "$SHUNT_URL/admin/api/pool/anthropic/accounts/$ACCOUNT_REF" \
  -H "x-shunt-admin-token: $ADMIN_TOKEN" \
  -H "content-type: application/json" \
  -d '{"paused": true}'
```

Set `"paused": false` to resume. See [`PATCH /admin/api/pool/{provider}/accounts/{account_ref}`](/reference/endpoints/) for the full endpoint reference.

## Ranking by soonest reset

`[server.pool] sort_by_reset` (default `false`) changes how the *available* tier is ordered: instead of largest projected burn-rate headroom, accounts sort by their earliest known quota reset (ascending — the account that recovers soonest is tried first; an account with no reset signal sorts last). The idea is to drain the account that will replenish earliest, keeping accounts with later resets in reserve as buffers.

`[server.pool]` — and therefore this setting — is process-wide, not per-provider, so toggling it affects every pooled provider at once.

Set it in `shunt.toml`:

```toml
[server.pool]
sort_by_reset = true
```

Or toggle it at runtime from the dashboard's checkbox above the pool table ("Rank available accounts by soonest quota reset instead of burn-rate headroom"), or directly:

```bash
curl -X PATCH "$SHUNT_URL/admin/api/pool" \
  -H "x-shunt-admin-token: $ADMIN_TOKEN" \
  -H "content-type: application/json" \
  -d '{"sort_by_reset": true}'
```

A runtime toggle overrides the config file's value until cleared or the process restarts, at which point the config file's own value applies again. To clear an override explicitly without restarting, send `null`:

```bash
curl -X PATCH "$SHUNT_URL/admin/api/pool" \
  -H "x-shunt-admin-token: $ADMIN_TOKEN" \
  -H "content-type: application/json" \
  -d '{"sort_by_reset": null}'
```

Omitting the field entirely is a no-op — it leaves the current override (or its absence) untouched; only an explicit `null` clears it.

`GET /admin/api/pool` reports the effective value (override or config) as a top-level `sort_by_reset` boolean. This setting has no effect at all while `[server.pool]` itself is absent — the legacy selection path it would otherwise change never runs — so both the runtime toggle and `GET /admin/api/pool`'s reported value are inert until `[server.pool]` exists.
