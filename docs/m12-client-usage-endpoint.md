# M12 — Client-facing usage endpoint (`GET /usage`)

M12 adds an opt-in, read-only **client-facing** endpoint that exposes a *sanitized, aggregated*
view of the shared account pool's quota state. Its purpose is transparency: a non-admin client
(a `[server.auth]` token holder) can see how close the shared pool is to a rate limit — per-window
remaining headroom and reset time — and anticipate throttling, instead of being surprised by a
`429`.

The only surface that previously showed usage was the admin dashboard
([M9](m9-admin-surface.md), `GET /admin/api/pool`), gated by the separate `[server.admin]` credential
and rendering full per-account detail. M12 gives ordinary clients a strictly narrower, redacted
slice of the same underlying data.

## Whose usage

The data is **shared-pool** state, not per-client accounting. shunt records per-account quota from
the Anthropic rate-limit headers, ChatGPT/Codex `x-codex-*` response headers, and the [usage-API
poller](m8-anthropic-multi-account.md); metrics are deliberately low-cardinality and never
client-scoped. So M12 reports the *pool's* headroom, not "your usage." Per-client accounting would
be a separate subsystem and is out of scope.

## Contrast with `GET /admin/api/pool`

| | `GET /admin/api/pool` (M9) | `GET /usage` (this milestone) |
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

- `remaining` — `mean(1 - utilization)` over **non-disabled** accounts that report the window: the
  fraction of the pool's combined capacity still unused, clamped to `0.0..=1.0` and rounded to four
  decimals. Nine exhausted accounts plus one fresh one read `0.1`, not `1.0`. This is a pool-wide
  aggregate, not a prediction of whether the next request will be admitted (routing also weighs
  availability, model, session affinity, and priority); for that question use the routing-aware
  worst case that `GET /api/oauth/usage` ([M14](m14-oauth-usage-endpoint.md)) reports. `null` only when
  no non-disabled account reports the window. ChatGPT/Codex response headers can
  populate the 5-hour and shared weekly windows, as does the WebSocket transport's in-stream
  `codex.rate_limits` event ([M7](m7-codex-websocket.md)), which reports them on every turn
  including turns on a reused connection; Codex has no Fable-scoped (`7d_oi`) signal, though
  another provider in a mixed pool may supply the aggregate Fable window.
- `resets_at` — the earliest window reset (unix epoch seconds) reported by the accounts counted in
  `remaining`: the soonest moment the aggregate can change. `null` when none of them reported one.

Plus a pool-level `status` derived purely from availability booleans (no numbers): `exhausted` when
every selectable (non-disabled) account is unavailable, `degraded` when any is near quota, else `ok`.

`pool` is the aggregate across **every** pooled provider. On a mixed pool (an Anthropic
`claude_oauth` upstream next to a Codex `chatgpt_oauth` one) that answers the wrong question for a
client whose traffic goes to one provider: `pool.windows.5h.remaining` blends both providers'
accounts into one mean, and `pool.status` is `ok` as long as any account of any provider is available. So the response
also carries `providers` — the same aggregate, computed over each provider's accounts only, keyed by
the configured provider name (issue #480), not an account identity. The key is always the provider's
own config-table name — the `<name>` in `[providers.<name>]`, or the `name` of a `[[upstreams]]`
entry — for every provider with a pooled auth mode, whether or not any route names it. Mapping a
model to that key is the client's job: `GET /routes` returns the `[[routes]]` table and so covers
only models explicitly listed there, no endpoint exposes the `[[models]].upstream_model`,
`[[route_prefixes]]`, or `server.default_provider` mappings, and `GET /v1/models` entries carry no
provider field. Providers whose auth mode is not pooled (`passthrough`,
`api_key`, …) are omitted, matching the filter `pool` already applies; with no pooled provider the
map is empty.

```json
{
  "pool": {
    "status": "ok",
    "windows": {
      "5h":    { "remaining": 0.21, "resets_at": 1751990000 },
      "7d":    { "remaining": 0.37, "resets_at": 1752400000 },
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
  `x-codex-*` headers, the WebSocket transport's in-stream `codex.rate_limits` event
  ([M7](m7-codex-websocket.md)), and the optional `GET /wham/usage` poller populate the 5-hour and
  shared weekly windows, and a window is `null` only when no non-disabled account has reported it.
  The headers only appear on a fresh WebSocket handshake, so on that transport the event is what
  keeps a reused connection's turns observed. The
  poller uses imported, refreshable accounts. For Codex, reset metadata is response-derived —
  from the `x-codex-*` reset headers and from the event's `reset_at`, which share one apply step:
  a response-supplied reset replaces the stored one, and when the response omits it a future
  stored reset survives while an elapsed stored reset for a reported window is
  cleared before fresh utilization is written; the wham report's parsed `reset_at` is not adopted
  as live reset metadata. Status metadata is header-only: the event carries no rate-limit-reached
  type, so the event path never writes `status` — though a status already stale by its own reset
  or observation bound still expires when that path runs. An authoritatively absent
  bucket still clears only its utilization and observation timestamp. Codex has no Fable-scoped (`7d_oi`) signal, although
  another provider in a mixed pool may supply the aggregate Fable value. The private endpoint is
  unofficial and opt-in through `usage_refresh_seconds`; fetch and parse failures preserve the
  prior state.
