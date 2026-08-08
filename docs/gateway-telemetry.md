# M-C — Inbound OTLP telemetry ingest

## Scope

M-C extends the opt-in `[server.gateway]` surface with authenticated inbound
OpenTelemetry ingest. It adds `POST /v1/metrics`, `POST /v1/logs`, and
`POST /v1/traces`, per-signal destination opt-in, and verbatim relay to the
configured collectors.

This closes the loop M-B opened. M-B pushes `OTEL_EXPORTER_OTLP_ENDPOINT` (set
to `[server.gateway].public_url`) and `OTEL_EXPORTER_OTLP_PROTOCOL` through
managed settings, which points every managed Claude Code client's exporter at
shunt. Before M-C those clients had nowhere to post; now the gateway accepts
their payloads and forwards them to the operator's collectors.

The existing gateway login and managed-settings routes are unchanged. When
`[server.gateway]` is absent, none of the ingest routes exist.

Issue: [#189](https://github.com/pleaseai/shunt/issues/189). Epic:
[#186](https://github.com/pleaseai/shunt/issues/186).

## Inbound ingest is not `[otel]` outbound export

Two unrelated OTLP surfaces live in this repository and must not be conflated:

| | `[otel]` (issue #64, `src/telemetry.rs`) | `[server.gateway.telemetry]` (this note, `src/gateway/telemetry_ingest.rs`) |
| :-- | :-- | :-- |
| Direction | Outbound | Inbound, then relayed outbound |
| Whose data | shunt's own metrics, traces, and logs | Claude Code clients' metrics, logs, and traces |
| Configured by | `[otel] endpoint` | `[server.gateway.telemetry] forward_to` |
| Effect on the HTTP surface | None | Registers three `POST` routes |

Enabling one does not enable the other, and they may point at different
collectors.

## Wire contract

| Request | Response |
| :-- | :-- |
| Valid gateway bearer, one or more destinations opted in to the signal | `200` with `{}`; payload relayed to each of them |
| Valid gateway bearer, no destination opted in to the signal (or no `telemetry` table) | `200` with `{}`; payload discarded |
| Valid gateway bearer, destination unreachable or answering non-2xx | `200` with `{}`; failure logged, never surfaced to the client |
| Missing, expired, or invalid gateway bearer | `401 authentication_error` |
| Body over the inbound size cap, or an interrupted upload | `413 request_too_large` |

Errors use the Anthropic error shape, like the rest of the gateway surface.

The response is always `200` on the accept path, and it never waits on a relay:
each destination is dispatched as a detached task. A client exporter must not
retry, back off, or surface an error because a collector behind the gateway is
slow or down.

Authentication is the same gateway bearer JWT that `GET /managed/settings`
requires — static `[server.auth]` client tokens do not authenticate these
routes. A config reload that removes `[server.gateway]` does **not** close the
surface: reload deliberately keeps the running JWT-auth capability and warns
that toggling the table requires a restart, so bearers issued before the reload
keep working until shunt restarts.

## Configuration

```toml
[server.gateway]
public_url = "https://gateway.example.com"

[server.gateway.telemetry]

# Metrics only (the default) to the primary collector.
[[server.gateway.telemetry.forward_to]]
url = "https://collector.example.com"

# A second destination that also takes logs and traces, with its own
# authentication header. The value is read literally from the config file, so
# render it there from the deployment's secret store.
[[server.gateway.telemetry.forward_to]]
url = "https://observability.internal.example.com/otlp"
logs = true
traces = true
headers = { "x-api-key" = "<OBSERVABILITY_API_KEY>" }
```

| Key | Default | Meaning |
| :-- | :-- | :-- |
| `url` | required | Base OTLP/HTTP endpoint; must be `http(s)` with a host |
| `headers` | none | Extra request headers applied to every relay to this destination |
| `metrics` | `true` | Relay `POST /v1/metrics` to this destination |
| `logs` | `false` | Relay `POST /v1/logs` to this destination |
| `traces` | `false` | Relay `POST /v1/traces` to this destination |

`url` is a **base** endpoint, matching the `OTEL_EXPORTER_OTLP_ENDPOINT`
convention the pushed client environment follows. shunt trims a trailing `/`
and appends the signal path, so `https://collector.example.com` receives
`https://collector.example.com/v1/metrics`. A base with its own path prefix
keeps it.

The three ingest routes are registered whenever `[server.gateway]` is present at
boot, independently of this table. A non-empty `forward_to` list is what enables
the managed telemetry environment push (M-B) and switches ingest from
accept-and-discard to relay. Edits to destinations and their per-signal flags
hot-apply on config reload. Adding or removing `[server.gateway]` itself still
requires a restart because route registration is fixed at boot.

## Relay semantics

Payloads are relayed **verbatim**: the exact inbound request bytes are POSTed
to each opted-in destination with no parsing, decoding, or re-encoding. Both
`application/x-protobuf` and `application/json` exporters therefore work
unchanged, as does a gzipped body.

Claude Code stamps the `user.id`, `user.email`, and `user.groups` attribution
attributes client-side, from the gateway-issued JWT, so shunt has nothing to
add — decoding and re-serializing would only risk dropping fields shunt does not
model.

Exactly two inbound headers cross to a destination:

- `content-type`, forwarded as received.
- `content-encoding`, forwarded when present.

The destination's own configured `headers` are applied after those as a map, so
an operator-configured value genuinely replaces the forwarded one for its key
rather than adding a second copy of the header. No other inbound header is
forwarded — in particular the client's `Authorization` gateway JWT is never sent
onward. Header names and values are validated at startup and on every reload, so
a malformed entry fails `shunt check` rather than being dropped at relay time.

Relay failures (connection error or a non-2xx response) are logged at `warn`
with the destination host, the signal, and the status. Header values, payload
bytes, and the full destination URL are never logged.

Relays do not follow redirects. reqwest's default policy strips only
`Authorization`- and `Cookie`-class headers across hosts, so a 3xx could
otherwise carry a destination's configured `x-api-key` to another host; a
collector has no legitimate reason to redirect, so a 3xx is reported through the
non-2xx path instead.

Each relay attempt is bounded at 30 seconds. Without it, a destination that
accepts a connection and then never answers would pin a detached task and its
payload bytes indefinitely — one per client flush.

Each accepted payload records one `shunt.gateway_telemetry_ingest` count tagged
with `signal` (`metrics`/`logs`/`traces`) and `outcome` (`relayed` when at least
one destination opted in, `discarded` when none did, `shed` when nothing could
be relayed because the in-flight relay limit was saturated, `rejected` for a
request refused before ingest). Both attributes are fixed strings, so the series
stay low-cardinality.

## Security notes

**Signal sensitivity drives the defaults.** Metrics are counters and timings, so
they default on. Logs and traces can carry command lines, prompts, tool inputs,
and file paths, so each destination must opt in to them explicitly. A signal no
destination opted in to is accepted and discarded rather than rejected, so a
client's exporter does not retry against a signal the operator deliberately
does not collect.

**No inbound header passthrough.** Relaying the client's `Authorization` header
would present a gateway-issued JWT to a third-party collector; relaying
arbitrary inbound headers would let a client reach a destination's own auth or
routing behavior. Only the two framing headers above cross over.

**Body cap and relay bound.** Inbound bodies are capped at 32 MiB, matching the
default inbound `limits.max_request_bytes` in the [Claude apps gateway
configuration reference](https://code.claude.com/docs/en/claude-apps-gateway-config)'s
HTTP tuning table. An over-cap body is rejected with `413` rather than
truncated, since a partial OTLP payload is not a valid one.

That cap alone bounds only one request. Because relays are detached, their
payload bytes outlive the request that accepted them — the inbound concurrency
permit releases as soon as the empty `{}` is written, while a relay can hold its
copy for up to the 30-second relay timeout. At most 64 relays may be in flight
at once, so worst-case resident payload memory is 64 × 32 MiB = 2 GiB, reached
only if every slot holds a maximum-size body simultaneously; real OTLP exports
are orders of magnitude smaller. Beyond that limit a payload is **shed** —
dropped with a `warn` log and an `outcome="shed"` count — rather than queued,
because waiting for a slot would put saturation back on the client's critical
path. Neither the cap nor the relay limit is a request rate limit.

**Destination URL shape.** A destination `url` must be a base endpoint: scheme,
host, and an optional path. A query string, fragment, or embedded userinfo is
rejected at startup — shunt appends `/v1/<signal>` by string concatenation, so a
query would end up on the wrong side of the join, and a URL-embedded credential
would reach error paths and logs.

**Destination secrets.** `headers` values are written in the config file. Treat
that file as a secret-bearing artifact, or render it from the deployment's
secret store.
