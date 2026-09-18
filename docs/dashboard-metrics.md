# Dashboard metrics — how the admin UI reads what shunt already measures

shunt records 18 metric series and can show none of them in its own dashboard.
This document records why, evaluates the ways out, and fixes three decisions —
**shunt owns the aggregate**, **history is a bounded in-memory ring**, and
**label cardinality is bounded before anything retains it** — so that a
durable-store decision stays a separate question rather than a prerequisite.

**No store is adopted here.** The persistence question is evaluated in
[`storage.md`](storage.md), whose Position ("adopt nothing yet") this document
does not disturb. Where the dashboard lives, what it may claim, and how it ships
are settled in [`admin-ui-delivery.md`](admin-ui-delivery.md).

## Motivation

`storage.md` names dashboard history as "the strongest reason to want a store at
all", and stops there. That framing skips a constraint that sits earlier in the
chain and is cheaper to fix: **shunt cannot read its own metrics back, at any
resolution, even for the current instant.** A store would give the dashboard
somewhere to put samples it has no way to obtain.

So the first question is not "where do samples live" but "who owns the
aggregate". Answer that and the dashboard renders live values immediately;
answer it well and history becomes a backing-store swap rather than a redesign.

## Current surface

Every instrument in `src/metrics.rs:39`, and whether anything in-process can
read it:

| Instrument | Type | Readable in-process |
| :-- | :-- | :-- |
| `shunt.requests` | `Counter<u64>` | no |
| `shunt.latency` | `Histogram<f64>` | no |
| `shunt.ttft` | `Histogram<f64>` | no |
| `shunt.stream_outcome` | `Counter<u64>` | no |
| `shunt.tokens` | `Counter<u64>` | no |
| `shunt.codex_continuation` | `Counter<u64>` | no |
| `shunt.codex_client_events` | `Counter<u64>` | no |
| `shunt.gateway_telemetry_ingest` | `Counter<u64>` | no |
| `shunt.upstream_retries` | `Counter<u64>` | no |
| `shunt.failover` | `Counter<u64>` | no |
| `shunt.stage_router.decisions` | `Counter<u64>` | no |
| `shunt.stage_router.flips` | `Counter<u64>` | no |
| `shunt.requests_shed` | `Counter<u64>` | no |
| `shunt.pool.rotations` | `Counter<u64>` | no |
| `shunt.pool.reprobes` | `Counter<u64>` | no |
| `shunt.codex_ws_overflow` | `Counter<u64>` | no |
| `shunt.pool.quota_utilization` | `ObservableGauge<f64>` | **yes** — `pool_utilization_values()` (`src/metrics.rs:62`) |
| `shunt.upstream.status` | `ObservableGauge<f64>` | **yes** — `upstream_status_values()` (`src/metrics.rs:74`) |

Sixteen of eighteen are write-only: `Counter` and `Histogram` handles expose
`add()` and `record()` and nothing that returns a value. The `*_for_tests`
helpers (`src/metrics.rs:342`, `src/metrics.rs:677`) are not a counter-example —
they read `Mutex<HashMap>` tables that the recording functions populate inside
a `#[cfg(test)]` block (`src/metrics.rs:319-327`), not the instruments. Those
tables do not exist in a release build.

The two exceptions are the shape this document generalizes. An observable gauge
inverts the ownership: shunt keeps the value in a `Mutex<HashMap>` it owns, and
the OTel callback *reads from it* at collection time. That is already the
arrangement the dashboard needs — it was simply never applied to the other
sixteen, because a gauge requires it and a counter does not.

### The provider is conditional

`src/main.rs:895` filters the config on `otel.enabled()` — a non-empty
`[otel] endpoint` — before calling `telemetry::init`. With no endpoint
configured, no meter provider is installed and `opentelemetry::global::meter()`
returns a no-op; the instruments are inert by construction, as the doc comment
at `src/metrics.rs:36` states.

This is what rules out the otherwise-obvious answer. Any design that reads
values back *through* the SDK inherits that condition, and a dashboard that only
draws charts once you have configured an external collector is not
self-monitoring — it is a second view of data you already exported.

## Alternatives

### A. `ManualReader` on the existing meter provider

`opentelemetry_sdk` 0.32.1 gates both types: `ManualReader` behind
`experimental_metrics_custom_reader` and `InMemoryMetricExporter` behind
`testing`. This crate takes the SDK's default features (`Cargo.toml:73`) and
enables neither, so neither type is reachable in the current build. A provider
may carry several readers, so the dashboard could collect on demand alongside
the periodic OTLP export.

Rejected on the conditional-provider constraint above: this works only where
`[otel]` is configured, which is the deployment that least needs it. The gating
is a second, independent blocker — adopting it would also mean turning on an
experimental SDK feature.

### B. Install a meter provider unconditionally, with only a `ManualReader`

Removes the condition by always building a provider, adding the OTLP periodic
reader only when `[otel]` is enabled.

Rejected, but on cost rather than correctness. It makes SDK aggregation
load-bearing for a user-facing surface: the dashboard's numbers become a
function of temporality selection, view configuration, and collection timing,
and "why does the chart disagree with Grafana" turns into an SDK question. It
also inverts the module's stated design — that both sinks are independently
opt-in and inert when unconfigured — to make one of them mandatory. The
aggregates the dashboard needs are small; borrowing a general aggregation
pipeline to compute them is not a saving.

### C. shunt owns the aggregate; both sinks read from it

The gauge pattern, applied uniformly. Adopted below.

## Decision 1 — shunt owns the aggregate

Every series keeps its current value in a shunt-owned structure. Sentry and OTel
become readers of that structure rather than its only home; the recording
functions in `src/metrics.rs` keep their present signatures, so no call site
changes.

Three properties follow, and each is independently worth the change:

- **The dashboard can render live values before any charting exists.** The
  `/admin/api/*` surface gains a read with no new dependency and no storage
  decision.
- **Both sinks stay opt-in and inert.** The aggregate exists whether or not
  `[otel]` or `[sentry]` is configured, which is what makes this
  *self*-monitoring.
- **Correctness stops depending on export.** Today a counter recorded with no
  provider installed is discarded. That is correct for an exporter and wrong for
  a gateway that should be able to answer "how many requests have I served".

The cost is a per-record lock acquisition on the request path. The gauges
already pay it, and the aggregate is a small map behind an uncontended
`Mutex` — but it is on the hot path, and `benches/stage_router.rs` is the
precedent for measuring rather than assuming.

## Decision 2 — history is a bounded in-memory ring, not a store

Retained samples go in a fixed-capacity ring of fixed-resolution buckets (the
working shape: 288 × 5 min = 24 h), behind one read API that the admin JSON
endpoints call.

What this buys is a dashboard that draws real charts while leaving **all four of
`storage.md`'s open questions unanswered** — including the first, "is
single-instance a documented limitation or a documented decision?", which that
document says every other question follows from. A ring buffer needs no schema,
no migration, and no answer on replicas.

Leaving a question unanswered is not the same as escaping it, and the fourth —
retention and PII — the ring inherits rather than defers. A fixed window is
itself a retention policy, and Decision 3 establishes that `model` is
client-controlled and passed through verbatim, so whatever a client puts in that
field is retained for the window and served over the admin API. Capping
cardinality bounds how many distinct labels are kept, not what any one of them
contains. The label set admitted to the ring therefore needs an explicit policy —
allowlist the configured ids, or redact or hash an unmatched one — and that
belongs with Decision 3's bounding rather than after it.

Its limitations are exact and should be documented rather than engineered
around: **history does not survive a restart, and the ring is process-local.**
For a gateway whose operator restarts it to change config, a day of in-memory
history is a genuine product, not a placeholder — but it is not an audit trail,
and nothing that needs durability (`/audit`, spend counters) may be built on it.
Across several processes each ring sees only its own replica's traffic, so a
chart drawn from it inherits the caveat
[`admin-ui-delivery.md`](admin-ui-delivery.md) already puts on
`/admin/api/pool`: it is this instance's view, not the fleet's, and the UI must
label it that way rather than implying a fleet-wide total.

The read API is the load-bearing part. Because the dashboard talks to it rather
than to the ring, `storage.md`'s eventual SQLite — should it be adopted on its
own merits — swaps in behind that API. This decision is therefore not a bet
against a store; it is what keeps the store optional.

## Decision 3 — bound label cardinality first

The ring is keyed by label set, so unbounded labels are not a collector's
problem here — they are a memory leak in shunt, reachable by a client that sends
a novel `model` string per request.

`src/metrics.rs:423` (`record_proxied_request`) passes `model` verbatim into both
sinks with no length cap and no cardinality bound. `sanitize_model_tag` exists
(`src/observability.rs:173`) but is called only on the Sentry span and event path
(`src/observability.rs:184`, `:285`, `:474`) — never from `src/metrics.rs`,
which contains no call to it.

Both call sites can carry a client-controlled string:

- `src/codex_endpoint.rs:456`, documented in place as "the model the client asked
  for".
- `src/proxy/failover.rs:207`, via `route.model` (`src/proxy/failover.rs:183`).
  For a **matched** `[[routes]]` or `[[models]]` entry that value is configured
  and therefore bounded. For an unmatched id it is not: both the prefix arm
  (`src/routing.rs:204-207`) and the terminal default-provider fallback
  (`src/routing.rs:209-216`) pass the requested id through as `Route.model`.
  `strip_context_window_hint` normalizes a trailing `[1m]` and bounds nothing
  else.

Two consequences worth recording, because both are drift that a reader would
otherwise have to rediscover:

- The module doc at `src/metrics.rs:19-22` states that attributes "stay
  low-cardinality (provider/model/status/…)". For `model` that is a design
  intent, not a property the code enforces.
- Issue #296 is titled `fix(codex): bound the inbound model metrics label
  length` and is open. The scope in that title is narrower than the behavior:
  the default-provider fallback on `/v1/messages` has the same property, so a
  fix confined to the Codex endpoint would close the issue while leaving the
  Anthropic path unbounded.

Bounding therefore belongs to Decision 1, not to a follow-up: a capped, sanitized
label set, with overflow folded into a single reserved bucket rather than
dropped, so a saturating client degrades the resolution of one series instead of
the memory of the process.

## What this does not decide

- **Whether shunt adopts a durable store.** [`storage.md`](storage.md) owns that,
  and its Position stands. This document's contribution is to remove the
  dashboard from the list of things waiting on it.
- **Which series earn a chart.** Eighteen sparklines is not a dashboard. The
  distinction the UI should draw is between series worth a time axis
  (requests/latency/ttft by provider, stream outcomes, pool utilization) and
  series better shown as a current count — but that is a design question for the
  UI, not a constraint from the metrics layer.
- **How a chart is rendered.** `ui/package.json` declares exactly `react` and
  `react-dom`; there is no charting dependency. Adding one, or hand-rolling SVG,
  is governed by [the frontend-stack decision in
  `admin-ui-delivery.md`](admin-ui-delivery.md#decision-6--the-frontend-stack-above-react--vite).

## Risks

- **A hot-path lock.** Decision 1 adds a lock acquisition per recorded series to
  request handling. Measure it; the gauges suggest it is affordable, which is not
  the same as knowing.
- **A second source of truth for the same numbers.** Once the dashboard shows
  request counts and a collector also does, the two can disagree — through
  sampling, restart, or ring eviction. The dashboard should state the window it
  is showing rather than imply an all-time total.
- **Ring parameters are a compatibility surface.** Resolution and depth are
  visible in what the UI can draw. Choosing them per-deployment via config
  multiplies the states to test; choosing them once does not.

## Open questions

1. Does the aggregate keep per-label histograms for `latency`/`ttft`, or only
   quantile-free summaries? A dashboard wants p50/p95; a bounded in-memory
   histogram per label set is materially more memory than a counter.
2. Is the ring's resolution fixed at compile time or configurable? See the third
   risk.
3. Does `/admin/api/*` expose the ring as one document, or per-series? The pool
   endpoint's backfill budget (`src/admin/mod.rs:1113`) is the precedent for
   bounding a dashboard read's cost.
4. Should the read API also back a future Prometheus exposition endpoint? Nothing
   here requires one, but the same aggregate would serve it, and deciding now
   affects whether the API is shaped around label sets or around charts.

## Testing

- The aggregate's readback is unit-testable without either sink configured —
  which is the property that distinguishes it from today's instruments, so a
  test asserting a recorded value is visible with no provider installed is the
  non-vacuous form. The existing `record_is_noop_without_sinks` family
  (`src/metrics.rs:692`) pins the *current* contract and must be revisited
  deliberately rather than deleted: "no export without a sink" stays true; "no
  value retained without a sink" is what changes.
- Cardinality bounding needs a positive test that a saturating label stream
  folds into the reserved bucket and leaves the map at its cap — an assertion
  that the map merely stays small is satisfied by a bug that drops everything.
- Ring eviction needs a test that crosses the capacity boundary, not one that
  fills it.

## Documentation impact

This is a design record; it changes no behavior, config key, endpoint, or CLI
surface, so under `AGENTS.md` it carries no README, `site/`, or locale updates on
its own. Implementing Decision 1 or 2 does: a new `/admin/api/*` read is an
endpoint change (`site/src/content/docs/reference/endpoints.md` plus its three
locale copies), and any ring parameter exposed in config reaches
`reference/configuration.md` the same way.

## Sources

- `src/metrics.rs`, `src/observability.rs`, `src/telemetry.rs`, `src/main.rs:895`,
  `src/routing.rs`, `src/proxy/failover.rs`, `src/codex_endpoint.rs`,
  `ui/package.json` — read at `6e7ccb13`.
- `opentelemetry_sdk` 0.32.1 — `manual_reader.rs`, `in_memory_exporter.rs`, and
  the `metrics` module's re-exports, read from the vendored crate source.
- Issue #296, open as of 2026-09-18.
