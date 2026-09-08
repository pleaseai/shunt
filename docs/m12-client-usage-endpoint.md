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
the Anthropic rate-limit headers, ChatGPT/Codex `x-codex-*` response headers, and the [usage-API
poller](m8-anthropic-multi-account.md); metrics are deliberately low-cardinality and never
client-scoped. So M12 reports the *pool's* headroom, not "your usage." Per-client accounting would
be a separate subsystem and is out of scope.

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
  `null` only when no non-disabled account reports the window. ChatGPT/Codex response headers can
  populate the 5-hour and shared weekly windows; Codex has no Fable-scoped (`7d_oi`) signal, though
  another provider in a mixed pool may supply the aggregate Fable window.
- `resets_at` — the least-utilized account's window reset (unix epoch seconds), when reported.

Plus a pool-level `status` derived purely from availability booleans (no numbers): `exhausted` when
every selectable (non-disabled) account is unavailable, `degraded` when any is near quota, else `ok`.

`pool` is the aggregate across **every** pooled provider. On a mixed pool (an Anthropic
`claude_oauth` upstream next to a Codex `chatgpt_oauth` one) that answers the wrong question for a
client whose traffic goes to one provider: `pool.windows.5h.remaining` is the best window across
both, and `pool.status` is `ok` as long as any account of any provider is available. So the response
also carries `providers` — the same aggregate, computed over each provider's accounts only, keyed by
the configured provider name (issue #480). The key is the upstream name that `/v1/models` routing
already exposes, not an account identity. Providers whose auth mode is not pooled (`passthrough`,
`api_key`, …) are omitted, matching the filter `pool` already applies; with no pooled provider the
map is empty.

```json
{
  "pool": {
    "status": "ok",
    "windows": {
      "5h":    { "remaining": 0.42, "resets_at": 1752000000 },
      "7d":    { "remaining": 0.61, "resets_at": 1752500000 },
      "fable": { "remaining": 0.85, "resets_at": 1753000000 }
    }
  },
  "providers": {
    "claude": {
      "status": "ok",
      "windows": {
        "5h":    { "remaining": 0.42, "resets_at": 1752000000 },
        "7d":    { "remaining": 0.61, "resets_at": 1752500000 },
        "fable": { "remaining": 0.85, "resets_at": 1753000000 }
      }
    },
    "codex": {
      "status": "exhausted",
      "windows": {
        "5h":    { "remaining": 0.0,  "resets_at": 1751990000 },
        "7d":    { "remaining": 0.12, "resets_at": 1752400000 },
        "fable": { "remaining": null, "resets_at": null }
      }
    }
  }
}
```

A per-provider `fable` window is `null` for a provider that has no Fable-scoped signal (Codex), even
when `pool.windows.fable` is populated by another provider.

Gateway-owned errors (a `401` for a missing/invalid client token, a `500` if the account store
cannot be read) use the Anthropic error shape, like the rest of the gateway.

## Boundaries

- **Sanitization is a test-enforced invariant.** A unit test asserts the serialized response never
  contains an account name, `priority`, `disabled`, `threshold`, `headroom`, or `cooldown` — in the
  nested `providers.<name>` entries as well as in `pool`. The only identifier a `providers` entry
  adds is its key, the configured provider name.
- **No per-client accounting.** The aggregate is pool-wide; it does not attribute usage to the
  calling client.
- **Codex usage is response- and poller-derived in this branch.** ChatGPT/Codex response
  `x-codex-*` headers and the optional `GET /wham/usage` poller populate the 5-hour and shared
  weekly windows, and a window is `null` only when no non-disabled account has reported it. The
  poller uses imported, refreshable accounts. For Codex, reset metadata remains header-derived:
  a future header reset is preserved, while an elapsed stored reset for a reported window is
  cleared before fresh utilization is written; the wham report's parsed `reset_at` is not adopted
  as live reset metadata, and status metadata remains header-derived. An authoritatively absent
  bucket still clears only its utilization and observation timestamp. Codex has no Fable-scoped (`7d_oi`) signal, although
  another provider in a mixed pool may supply the aggregate Fable value. The private endpoint is
  unofficial and opt-in through `usage_refresh_seconds`; fetch and parse failures preserve the
  prior state.
