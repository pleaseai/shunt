# M12 — Client-facing usage endpoint (`GET /usage`)

M12 adds an opt-in, read-only **client-facing** endpoint that exposes a *sanitized, aggregated*
view of the shared account pool's quota state. Its purpose is transparency: a non-admin client
(a `[server.auth]` token holder) can see how close the shared pool is to a rate limit — per-window
remaining headroom and reset time — and anticipate throttling, instead of being surprised by a
`429`.

The only surface that previously showed usage was the admin dashboard
([M9](m9-admin-surface.md), `GET /admin/pool`), gated by the separate `[server.admin]` credential
and rendering full per-account detail. M12 gives ordinary clients a strictly narrower, redacted
slice of the same underlying data.

## Whose usage

The data is **shared-pool** state, not per-client accounting. shunt records per-account quota from
the Anthropic rate-limit headers and the [usage-API poller](m8-anthropic-multi-account.md); metrics
are deliberately low-cardinality and never client-scoped. So M12 reports the *pool's* headroom, not
"your usage." Per-client accounting would be a separate subsystem and is out of scope.

## Contrast with `GET /admin/pool`

| | `GET /admin/pool` (M9) | `GET /usage` (this milestone) |
| :-- | :-- | :-- |
| Auth | `[server.admin]` admin token / browser session | `[server.auth]` client token (header, `x-api-key`, or `Authorization: Bearer`) |
| Audience | Operator | Any authenticated client |
| Granularity | Per account: name, priority, `disabled`, cooldown, utilization, headroom, status | Pool aggregate only |
| Account identity | Exposed | **Never** — no name, count, priority, `disabled`, threshold, or headroom |
| Registered when | `[server.admin]` present | `[server.usage]` present (which requires `[server.auth]`) |

Both read the same `AccountPool::snapshot` output; `GET /usage` collapses it to an aggregate and
drops every identifying field.

## Configuration

A new opt-in `[server.usage]` table, mirroring the [M9](m9-admin-surface.md) `[server.admin]` and
[M11](m11-inbound-codex-endpoint.md) `[server.codex_endpoint]` opt-in pattern. Presence alone opts
in; the table has no keys today.

```toml
[server.usage]
```

It **requires `[server.auth]`**: the endpoint must identify its caller by client token, so a
`[server.usage]` set without `[server.auth]` fails startup (`ConfigError::UsageEndpointRequiresAuth`)
rather than serving pool telemetry unauthenticated. The route is registered once at boot when the
table is present; a config reload only re-resolves the client tokens it authenticates against.

## Response

Per tracked window — the rolling 5-hour session window (`5h`), the shared weekly window (`7d`), and
the Fable-scoped weekly window (`fable` / `7d_oi`):

- `remaining` — `1 - min(utilization)` over **non-disabled** accounts that report the window: the
  least reported utilization among non-disabled accounts, clamped to `0.0..=1.0` and rounded to four
  decimals. This is a pool-wide aggregate, not a prediction of which account the next request will
  actually route to (routing also weighs availability, model, session affinity, and priority).
  `null` when no account reports the window (e.g. the Codex backend, which publishes no quota
  headers).
- `resets_at` — the least-utilized account's window reset (unix epoch seconds), when reported.

Plus a pool-level `status` derived purely from availability booleans (no numbers): `exhausted` when
every selectable (non-disabled) account is unavailable, `degraded` when any is near quota, else `ok`.

```json
{
  "pool": {
    "status": "ok",
    "windows": {
      "5h":    { "remaining": 0.42, "resets_at": 1752000000 },
      "7d":    { "remaining": 0.61, "resets_at": 1752500000 },
      "fable": { "remaining": null, "resets_at": null }
    }
  }
}
```

Gateway-owned errors (a `401` for a missing/invalid client token, a `500` if the account store
cannot be read) use the Anthropic error shape, like the rest of the gateway.

## Boundaries

- **Sanitization is a test-enforced invariant.** A unit test asserts the serialized response never
  contains an account name, `priority`, `disabled`, `threshold`, `headroom`, or `cooldown`.
- **No per-client accounting.** The aggregate is pool-wide; it does not attribute usage to the
  calling client.
- **Codex is blank by design.** No quota headers upstream ⇒ `null` windows, the same limitation the
  admin dashboard carries.
