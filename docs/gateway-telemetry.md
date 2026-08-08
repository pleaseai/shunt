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
| Body over the inbound size cap, or an interrupted upload | `413 invalid_request_error` |

Errors use the Anthropic error shape, like the rest of the gateway surface.

The response is always `200` on the accept path, and it never waits on a relay:
each destination is dispatched as a detached task. A client exporter must not
retry, back off, or surface an error because a collector behind the gateway is
slow or down.

Authentication is the same gateway bearer JWT that `GET /managed/settings`
requires — static `[server.auth]` client tokens do not authenticate these
routes. A config reload that removes `[server.gateway]` leaves the
boot-registered routes in place with no auth to check against; they then fail
closed with `401`.

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

A non-empty `forward_to` list is what enables both the managed telemetry
environment push (M-B) and this ingest path; there is no separate switch. Edits
to destinations and their per-signal flags hot-apply on config reload. Adding or
removing `[server.gateway]` itself still requires a restart because route
registration is fixed at boot.

## Relay semantics

Payloads are relayed **verbatim**: the exact inbound request bytes are POSTed
to each opted-in destination with no parsing, decoding, or re-encoding. Both
`application/x-protobuf` and `application/json` exporters therefore work
unchanged, as does a gzipped body.

Claude Code stamps its attribution attributes (user, session, organization)
client-side, so shunt has nothing to add — decoding and re-serializing would
only risk dropping fields shunt does not model.

Exactly two inbound headers cross to a destination:

- `content-type`, forwarded as received.
- `content-encoding`, forwarded when present.

The destination's own configured `headers` are applied after those, so an
operator-configured value is authoritative for its key. No other inbound header
is forwarded — in particular the client's `Authorization` gateway JWT is never
sent onward.

Relay failures (connection error or a non-2xx response) are logged at `warn`
with the destination host, the signal, and the status. Header values and
payload bytes are never logged.

Each relay attempt is bounded at 30 seconds. The bound is per request rather
than on shunt's shared HTTP client, which is deliberately timeout-free because
it also carries streaming inference. Without it, a destination that accepts a
connection and then never answers would pin a detached task and its payload
bytes indefinitely — one per client flush.

Each accepted payload records one `shunt.gateway_telemetry_ingest` count tagged
with `signal` (`metrics`/`logs`/`traces`) and `outcome` (`relayed` when at least
one destination opted in, `discarded` when none did, `rejected` for a request
refused before ingest). Both attributes are fixed strings, so the series stay
low-cardinality.

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

**Body cap.** Inbound bodies are capped at 32 MiB, matching the reference Claude
apps gateway's default inbound `limits.max_request_bytes`. An over-cap body is
rejected with `413` rather than truncated, since a partial OTLP payload is not a
valid one. The cap bounds per-request memory; it is not a rate limit.

**Destination secrets.** `headers` values are written in the config file. Treat
that file as a secret-bearing artifact, or render it from the deployment's
secret store.
