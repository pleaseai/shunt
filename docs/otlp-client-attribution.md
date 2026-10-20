# OTLP client attribution — zero-config monitoring without double counting

This note records why shunt's server-derived metrics carry `client_kind` and `managed`
attributes instead of new metric names or an operator setting, so a collector that
receives both shunt's own `shunt.*` series and a relayed Claude Code `claude_code.*`
signal can be read without double counting.

## Problem

shunt exposes two unrelated OTLP surfaces (see the [M-C telemetry note](gateway-telemetry.md)):

| Surface | Direction | Whose data | Needs client config? |
| :-- | :-- | :-- | :-- |
| `[otel]` | outbound | shunt's own `shunt.*` series, derived from proxied traffic | no |
| `[server.gateway.telemetry]` | inbound relay | the client's own `claude_code.*` OTLP payloads | no for gateway-mode Claude Code (managed settings push `OTEL_*`); n/a for everything else |

Codex never routes OpenTelemetry through shunt: its `[otel]` block is off by
default, and the only client-side product telemetry shunt sees is the
analytics-events batch, which lands in a discard sink counted as
`shunt.codex_client_events` (see the [inbound Codex endpoint note](m11-inbound-codex-endpoint.md)). For Codex,
and for Claude Code pointed at shunt through a plain `ANTHROPIC_BASE_URL`, the
server-derived `shunt.*` series are therefore the only zero-config monitoring
path.

That leaves one seam where the same fact is reported twice. A gateway-mode
Claude Code client reads the API `usage` block and exports
`claude_code.token.usage`; shunt reads the same `usage` block off the same
stream and records `shunt.tokens`. Same numbers, two series. Nothing collides
in a backend, but a dashboard that sums "tokens" across both double counts, and
the moment a server-derived series is *named* `claude_code.*` to look like the
client's, the duplicate becomes real.

## Decision

1. **Server-derived series stay under `shunt.*`.** shunt never emits a
   `claude_code.*` or `codex.*` metric name. The relayed client signal remains
   the richer one (cost, lines of code, commits, tool decisions, active time,
   prompt-level logs exist only client-side); `shunt.*` is positioned as
   coverage for clients that report nothing themselves, not as a replacement.

2. **Disambiguate with attributes, not new names or a config knob.** The five
   per-request series gain two fixed-vocabulary attributes:

   | Attribute | Values |
   | :-- | :-- |
   | `client_kind` | `claude-code`, `codex`, `other` |
   | `managed` | `true`, `false` |

   `client_kind` is classified from request headers (values and user-agent prefixes match
   case-insensitively), and the raw header value is never
   exported:

   - `x-app: cli`, or a `user-agent` starting with `claude-cli/` → `claude-code`
   - an `originator` header, or a `user-agent` starting with `codex_` → `codex`
   - anything else → `other`

   `managed` is `true` when the request authenticated with a gateway-issued JWT,
   the same credential that makes the client eligible for the pushed `OTEL_*`
   environment. Static `[server.auth]` tokens, verify-only JWTs from an
   external identity provider, admin keys, and unauthenticated passthrough are
   `false`: none of them can fetch managed settings, so none of them receives
   the pushed exporter environment. The inbound Codex endpoint, over HTTP and over its
   Responses WebSocket alike, authenticates only `[server.auth]` credentials
   and never checks a gateway-issued JWT, so every request it serves reports
   `managed="false"`.

   A dashboard then takes `claude_code.*` where it exists and
   `shunt.*{managed="false"}` everywhere else, with no operator setting to keep
   in sync. That filter is only correct when the backend actually receives the
   managed clients' `claude_code.*` metrics, either relayed through a
   `forward_to` destination with `metrics` on or exported by the clients
   directly. When it does not, the dashboard reads `shunt.*` across both
   `managed` values instead; filtering to `managed="false"` there would drop
   every managed request. Which case applies follows from the managed-settings
   push: when some destination opts in to logs or traces but none to metrics,
   managed clients are pushed `OTEL_METRICS_EXPORTER=none` and report no
   metrics; when no destination opts in to any signal, nothing is pushed and
   each client keeps its own exporter configuration. `managed` is a statement about the credential, not about whether
   `[server.gateway.telemetry].forward_to` is currently non-empty: the latter
   hot-reloads and is not observable per request, and a client whose exporter
   was pushed as `none` still holds a gateway JWT.

3. **Which series carry them.** `shunt.requests`, `shunt.latency`,
   `shunt.ttft`, `shunt.stream_outcome`, and `shunt.tokens` — every series that
   describes one client request. Pool, failover, retry, continuation, and
   ingest counters describe shunt-internal events and stay as they are. The
   same two fields are recorded on the `proxy_request` and
   `codex_endpoint_request` spans as `shunt.client_kind` and `shunt.managed`
   so traces can be filtered the same way. An inbound Codex WebSocket turn has
   no request span, so its attribution appears on the metrics only.

   The name is `client_kind` rather than `client` because `shunt.requests`
   and `shunt.latency` already carry a `caller` attribute whose values are
   `client` (a client turn) and `router` (an internal judge call made by a
   driven `[models.router]`).

4. **Judge calls are always `managed="false"`.** A judge call is shunt's own
   traffic. It rides the same failover chain and lands in the same series as
   the client turn that triggered it, but no client exporter ever reports it.
   Tagging it with the originating turn's `managed="true"` would drop it from
   the dedupe rule above and undercount. It keeps the originating turn's
   `client_kind` where that is known, so judge cost can still be split by
   client.

5. **Cardinality.** Both attributes are closed enumerations (3 × 2), so the
   worst case multiplies each existing `(provider, model, …)` series by six.
   User and session identity are deliberately *not* metric attributes: the
   existing `[otel] include_session_id` gate puts the session id on spans only,
   and any user-level attribution follows that precedent (see follow-ups).

## What is not duplicated

- **Retries and failover.** One client request can be several upstream
  attempts; `shunt.requests` counts attempts, a client counts one call. That is
  a semantic difference, not a duplicate, and `shunt.upstream_retries` and
  `shunt.failover` already separate it.
- **Cache tokens.** `shunt.tokens{kind="cache_read"|"cache_creation"}` already
  exist; the client reports the same split under its own names.
- **A client that already exports directly** (an operator-side
  `OTEL_EXPORTER_OTLP_ENDPOINT`) is not double-reporting once it enters gateway
  mode: the managed-settings environment replaces the ambient endpoint, so the
  client posts only to shunt. Pointing both `[otel]` and `forward_to` at the
  same collector simply lands `shunt.*` and `claude_code.*` side by side, which
  is the case the attributes above resolve.
- **Codex with its own `[otel]` enabled** sends `codex.*` straight to a
  collector next to `shunt.*`. shunt cannot dedupe a signal it never sees; the
  `client_kind="codex"` attribute lets the dashboard pick one side.

## Follow-ups (not in this change)

- Per-user attribution on spans and logs from Claude Code's `metadata.user_id`
  and the Codex `session-id` header, behind the same opt-in shape as
  `include_session_id`. New public config key — needs its own review.
- Cost as a server-derived series requires a price table shunt does not carry.

## Related

- [M-C telemetry note](gateway-telemetry.md) — inbound relay and the managed `OTEL_*` push.
- [Running shunt, metric series](running.md) — the metric series reference (attribute
  table updated in the same PR as this note).
- [Inbound Codex endpoint note](m11-inbound-codex-endpoint.md) — Codex analytics sink.
