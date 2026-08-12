# Upstream status polling

## Scope

`[server.status]` is an opt-in, observation-only background poller for
provider Statuspage `summary.json` endpoints. It never feeds routing,
failover, or pool/cooldown behavior — its only effect is to update a shared
store surfaced by the `shunt.upstream.status` metric and the admin dashboard's
"Upstream status" strip. When the table is absent, or `sources` is empty, no
background task is started and nothing about request handling changes.

## Configuration

```toml
[server.status]
refresh_seconds = 300

[[server.status.sources]]
provider = "claude"
url = "https://status.claude.com/api/v2/summary.json"

[[server.status.sources]]
provider = "openai"
url = "https://status.openai.com/api/v2/summary.json"
```

`refresh_seconds` defaults to 300; a positive value below 60 is clamped up to
a 60-second floor (mirroring `[server.pool] usage_refresh_seconds`), and `0`
disables polling. Each `sources` entry needs a non-empty, unique `provider`
label and an `http`/`https` `url`. Configuration validation fails closed: an
empty or duplicate `provider`, or an unparseable/non-`http(s)` `url`, makes
`shunt check` (and boot) fail — full detail in
[`docs/reference/configuration.md`](../site/src/content/docs/reference/configuration.md#serverstatus-optional).

Whether the poller starts at all, and its interval, are decided once from the
boot config, exactly like the usage poller: absent, empty, or
`refresh_seconds = 0` at boot means no task is ever spawned, and a later
reload that enables `[server.status]` does not retroactively start one. Once
running, each tick re-reads the current `sources` list from the live
(possibly reloaded) config, so edits to which sources are polled take effect
from the next tick onward.

## Fail-open runtime, fail-closed config

Config validation is deliberately strict (fail closed) so a broken
`[server.status]` is caught at boot rather than silently degrading. Runtime
polling is deliberately permissive (fail open): a transport error, non-2xx
response, oversized body (capped at 1 MiB, rejected while streaming rather
than truncated and parsed), invalid JSON, or an unrecognized `indicator`
string in the response all resolve to `Indicator::Unknown` — "no signal" —
rather than `Indicator::None` — "operational."

This distinction is the reason the feature exists in this shape: a failed
poll must never silently report an all-clear for a source shunt could not
actually reach. Concretely, a failed poll always *replaces* any previously
good stored value with `Unknown` rather than leaving the stale "operational"
reading in place, and one source's failure cannot stop the others in the same
tick from polling. Errors are truncated to roughly 200 characters before
being stored.

## Admin surface

`GET /admin/status` (admin-authenticated, same auth as `GET /admin/pool`)
returns each configured source's most recently observed indicator,
description, incidents, and observed timestamp:

```json
{
  "sources": [
    {
      "provider": "claude",
      "indicator": "minor",
      "description": "Partially Degraded Service",
      "incidents": [],
      "observed_at": 1755000000,
      "error": null
    }
  ]
}
```

An unconfigured or empty `[server.status]` reports an empty `sources` array.
The admin dashboard reads that as "hide the whole section" rather than
rendering an empty table.

## Metric

The observable gauge `shunt.upstream.status` reports severity per provider
(`provider` label only, to keep cardinality low): `0` none, `1` minor, `2`
major, `3` critical. A provider whose current entry is `Unknown` is omitted
from the gauge's collected series entirely — it never reports a `0` sample —
so "no signal" cannot be misread as "operational" on a dashboard or alert
rule. The same value is also emitted as a Sentry gauge metric when
`[sentry] metrics_enabled` is on.
