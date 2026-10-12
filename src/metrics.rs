//! Usage/performance and pool-health metric emission.
//!
//! Every series is recorded to two independent, opt-in sinks — each a no-op
//! unless its section is configured:
//!
//! - **Sentry** (`[sentry] metrics = true`): counters, distributions, and gauges
//!   are dropped by the SDK when no client is bound or `enable_metrics` is off.
//! - **OpenTelemetry** (`[otel]` with `metrics = true`): the same series use the
//!   global meter. A no-op until `crate::telemetry::init` installs a meter
//!   provider, so with `[otel]` absent the instruments are inert.
//!
//! Request metrics cover request counts/header latency, streaming TTFT/outcomes
//! and streaming token usage, Codex continuation decisions, Codex WebSocket
//! dedicated-overflow admission outcomes, and sanitized client analytics event
//! names, retries, and requests shed at the inbound concurrency limit. Pool
//! metrics expose best-account quota utilization and account rotations. Gateway
//! metrics count inbound OTLP telemetry payloads by signal and ingest outcome.
//!
//! Attributes stay low-cardinality (provider/model/status/outcome/kind/window/
//! reason/signal, plus the sanitized, cardinality-capped `event` on
//! `shunt.codex_client_events`) — never client names, account ids, session ids,
//! or anything else request-derived. Token metrics currently cover streaming
//! responses only; non-streaming token usage is intentionally out of scope.

use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use opentelemetry::{
    metrics::{Counter, Histogram, ObservableGauge},
    KeyValue,
};
use sentry::protocol::Unit;

/// OTel instruments on the global meter. Created lazily on first record so the
/// meter provider (installed at startup, before any request) is already in
/// place; with `[otel]` disabled the global meter is a no-op and so are these.
struct OtelInstruments {
    requests: Counter<u64>,
    latency: Histogram<f64>,
    ttft: Histogram<f64>,
    stream_outcome: Counter<u64>,
    tokens: Counter<u64>,
    continuation: Counter<u64>,
    codex_client_events: Counter<u64>,
    gateway_telemetry_ingest: Counter<u64>,
    upstream_retries: Counter<u64>,
    failover: Counter<u64>,
    stage_decisions: Counter<u64>,
    router_decisions: Counter<u64>,
    judge_calls: Counter<u64>,
    stage_flips: Counter<u64>,
    requests_shed: Counter<u64>,
    _pool_utilization: ObservableGauge<f64>,
    pool_rotations: Counter<u64>,
    pool_reprobes: Counter<u64>,
    codex_ws_overflow: Counter<u64>,
    _upstream_status: ObservableGauge<f64>,
}

type PoolUtilizationValues = HashMap<(String, &'static str), Option<f64>>;

fn pool_utilization_values() -> &'static Mutex<PoolUtilizationValues> {
    static VALUES: OnceLock<Mutex<PoolUtilizationValues>> = OnceLock::new();
    VALUES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Current `shunt.upstream.status` severity per provider (`[server.status]`).
/// A provider absent from this map reports no sample on the next collection
/// — used for `Indicator::Unknown` ("no signal"), which must never surface as
/// a `0` (`Indicator::None`, "operational") sample. See
/// [`crate::upstream_status::Indicator`].
type UpstreamStatusValues = HashMap<String, f64>;

fn upstream_status_values() -> &'static Mutex<UpstreamStatusValues> {
    static VALUES: OnceLock<Mutex<UpstreamStatusValues>> = OnceLock::new();
    VALUES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn otel_instruments() -> &'static OtelInstruments {
    static INSTRUMENTS: OnceLock<OtelInstruments> = OnceLock::new();
    INSTRUMENTS.get_or_init(|| {
        let meter = opentelemetry::global::meter(crate::telemetry::SCOPE);
        OtelInstruments {
            requests: meter
                .u64_counter("shunt.requests")
                .with_description("Proxied inference requests")
                .build(),
            latency: meter
                .f64_histogram("shunt.latency")
                .with_unit("ms")
                .with_description("Proxied inference request latency")
                .build(),
            ttft: meter
                .f64_histogram("shunt.ttft")
                .with_unit("ms")
                .with_description("Time from request start to the first SSE body chunk")
                .build(),
            stream_outcome: meter
                .u64_counter("shunt.stream_outcome")
                .with_description("How proxied SSE response streams ended")
                .build(),
            tokens: meter
                .u64_counter("shunt.tokens")
                .with_description("Token usage reported by proxied SSE streams")
                .build(),
            continuation: meter
                .u64_counter("shunt.codex_continuation")
                .with_description(
                    "Codex WebSocket continuation decisions (hit vs full-input fallback)",
                )
                .build(),
            codex_client_events: meter
                .u64_counter("shunt.codex_client_events")
                .with_description("Sanitized Codex client analytics event counts")
                .build(),
            gateway_telemetry_ingest: meter
                .u64_counter("shunt.gateway_telemetry_ingest")
                .with_description(
                    "Inbound gateway OTLP payloads by signal and ingest outcome (issue #189)",
                )
                .build(),
            upstream_retries: meter
                .u64_counter("shunt.upstream_retries")
                .with_description(
                    "Bounded upstream retries issued for transient failures (issue #48)",
                )
                .build(),
            failover: meter
                .u64_counter("shunt.failover")
                .with_description("Ordered upstream failover state transitions")
                .build(),
            stage_decisions: meter
                .u64_counter("shunt.stage_router.decisions")
                .with_description(
                    "Stage-router tier decisions by routed model, tier, and decision source",
                )
                .build(),
            router_decisions: meter
                .u64_counter("shunt.router.decisions")
                .with_description(
                    "[models.router] decisions by routed model, algorithm, target, and source",
                )
                .build(),
            judge_calls: meter
                .u64_counter("shunt.router.judge_calls")
                .with_description(
                    "[models.router] judge consultations by routed model, algorithm, and outcome",
                )
                .build(),
            stage_flips: meter
                .u64_counter("shunt.stage_router.flips")
                .with_description(
                    "Stage-router decisions that moved a session off its pinned tier",
                )
                .build(),
            requests_shed: meter
                .u64_counter("shunt.requests_shed")
                .with_description("Inbound requests rejected at the concurrency limit (issue #260)")
                .build(),
            _pool_utilization: meter
                .f64_observable_gauge("shunt.pool.quota_utilization")
                .with_description("Least quota utilization among enabled pool accounts")
                .with_callback(|observer| {
                    let values = pool_utilization_values()
                        .lock()
                        .expect("pool utilization metric lock poisoned");
                    for ((provider, window), value) in values.iter() {
                        let Some(value) = value else {
                            continue;
                        };
                        observer.observe(
                            *value,
                            &[
                                KeyValue::new("provider", provider.clone()),
                                KeyValue::new("window", *window),
                            ],
                        );
                    }
                })
                .build(),
            pool_rotations: meter
                .u64_counter("shunt.pool.rotations")
                .with_description("Account-pool rotations by low-cardinality reason")
                .build(),
            pool_reprobes: meter
                .u64_counter("shunt.pool.reprobes")
                .with_description(
                    "Opportunistic re-probes of a stale near-quota Codex/ChatGPT account; WebSocket-enabled providers count inbound HTTP probes only",
                )
                .build(),
            codex_ws_overflow: meter
                .u64_counter("shunt.codex_ws_overflow")
                .with_description(
                    "Codex WebSocket dedicated overflow connections (opened vs refused at the ceiling, issue #248)",
                )
                .build(),
            _upstream_status: meter
                .f64_observable_gauge("shunt.upstream.status")
                .with_description(
                    "Observed upstream provider Statuspage severity (0=none, 1=minor, 2=major, 3=critical); omitted, not zero, when unknown ([server.status])",
                )
                .with_callback(|observer| {
                    let values = upstream_status_values()
                        .lock()
                        .expect("upstream status metric lock poisoned");
                    for (provider, value) in values.iter() {
                        observer.observe(*value, &[KeyValue::new("provider", provider.clone())]);
                    }
                })
                .build(),
        }
    })
}

/// Record time from request start to the first successfully forwarded SSE body
/// chunk. Non-streaming responses do not call this function. Emitted to Sentry
/// and OpenTelemetry; each sink is inert unless configured.
pub fn record_ttft(provider: &str, model: &str, milliseconds: f64) {
    sentry::metrics::distribution("shunt.ttft", milliseconds)
        .unit(Unit::Millisecond)
        .attribute("provider", provider.to_owned())
        .attribute("model", model.to_owned())
        .capture();

    let attributes = [
        KeyValue::new("provider", provider.to_owned()),
        KeyValue::new("model", model.to_owned()),
    ];
    otel_instruments().ttft.record(milliseconds, &attributes);
}

/// Record the final outcome of one SSE response stream. `outcome` is one of
/// `completed`, `error_event`, `upstream_cut`, or `client_disconnect`; callers
/// guarantee exactly one record per stream.
pub fn record_stream_outcome(provider: &str, model: &str, outcome: &'static str) {
    sentry::metrics::counter("shunt.stream_outcome", 1)
        .attribute("provider", provider.to_owned())
        .attribute("model", model.to_owned())
        .attribute("outcome", outcome.to_owned())
        .capture();

    let attributes = [
        KeyValue::new("provider", provider.to_owned()),
        KeyValue::new("model", model.to_owned()),
        KeyValue::new("outcome", outcome),
    ];
    otel_instruments().stream_outcome.add(1, &attributes);
}

/// Add one last-seen token count from a completed or interrupted SSE stream.
/// `kind` is one of `input`, `output`, `cache_read`, or `cache_creation`; absent
/// usage fields are not emitted by the stream observer.
pub fn record_stream_tokens(provider: &str, model: &str, kind: &'static str, count: u64) {
    sentry::metrics::counter("shunt.tokens", count as f64)
        .attribute("provider", provider.to_owned())
        .attribute("model", model.to_owned())
        .attribute("kind", kind.to_owned())
        .capture();

    let attributes = [
        KeyValue::new("provider", provider.to_owned()),
        KeyValue::new("model", model.to_owned()),
        KeyValue::new("kind", kind),
    ];
    otel_instruments().tokens.add(count, &attributes);
}

/// Replace the current quota utilization for one provider/window series. `None`
/// suppresses the series from subsequent OpenTelemetry collections after the
/// last eligible account is disabled, removed, or its window expires.
pub fn record_pool_utilization(provider: &str, window: &'static str, utilization: Option<f64>) {
    match utilization {
        Some(utilization) => {
            sentry::metrics::gauge("shunt.pool.quota_utilization", utilization)
                .attribute("provider", provider.to_owned())
                .attribute("window", window.to_owned())
                .capture();
            pool_utilization_values()
                .lock()
                .expect("pool utilization metric lock poisoned")
                .insert((provider.to_owned(), window), Some(utilization));
        }
        None => {
            pool_utilization_values()
                .lock()
                .expect("pool utilization metric lock poisoned")
                .remove(&(provider.to_owned(), window));
        }
    }
    let _ = otel_instruments();
}

/// Test-only observation point for [`record_pool_utilization`]. The production
/// gauge is callback-driven, so focused pool tests read the in-process value
/// map to verify that each provider alias receives an update.
#[cfg(test)]
pub(crate) fn pool_utilization_value_for_tests(
    provider: &str,
    window: &'static str,
) -> Option<f64> {
    pool_utilization_values()
        .lock()
        .expect("pool utilization metric lock poisoned")
        .get(&(provider.to_owned(), window))
        .copied()
        .flatten()
}

/// Record one move away from a pool account, or one request that found the pool
/// exhausted. Reasons are deliberately low-cardinality and account-free.
pub fn record_pool_rotation(provider: &str, reason: &'static str) {
    sentry::metrics::counter("shunt.pool.rotations", 1)
        .attribute("provider", provider.to_owned())
        .attribute("reason", reason.to_owned())
        .capture();

    let attributes = [
        KeyValue::new("provider", provider.to_owned()),
        KeyValue::new("reason", reason),
    ];
    otel_instruments().pool_rotations.add(1, &attributes);
}

/// Record one opportunistic re-probe: a stale near-quota Codex/ChatGPT
/// account promoted to the front of selection so it takes live traffic and
/// refreshes its observed quota. Provider-only and account-free, like
/// [`record_pool_rotation`] — the probe-selection log line carries the
/// account name for anything that needs finer granularity. For a provider
/// with WebSocket enabled, outbound Responses selection does not probe, so
/// this provider-labelled counter records inbound HTTP probes only.
pub fn record_pool_reprobe(provider: &str) {
    #[cfg(test)]
    {
        *test_pool_reprobe_counts()
            .lock()
            .expect("test pool reprobe counter lock poisoned")
            .entry(provider.to_owned())
            .or_insert(0) += 1;
    }

    sentry::metrics::counter("shunt.pool.reprobes", 1)
        .attribute("provider", provider.to_owned())
        .capture();

    let attributes = [KeyValue::new("provider", provider.to_owned())];
    otel_instruments().pool_reprobes.add(1, &attributes);
}

/// Test-only observation point for [`record_pool_reprobe`]. The production
/// sinks are intentionally opaque, so tests use a provider-keyed in-process
/// counter to assert that dispatch, rather than selection, accounted for the
/// probe.
#[cfg(test)]
pub fn pool_reprobe_count_for_tests(provider: &str) -> u64 {
    *test_pool_reprobe_counts()
        .lock()
        .expect("test pool reprobe counter lock poisoned")
        .get(provider)
        .unwrap_or(&0)
}

#[cfg(test)]
fn test_pool_reprobe_counts() -> &'static Mutex<HashMap<String, u64>> {
    static COUNTS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Replace the current `shunt.upstream.status` severity for one provider
/// (`[server.status]`). `severity` is [`crate::upstream_status::Indicator::severity`]:
/// `None` — i.e. `Indicator::Unknown`, "we have no signal" — removes the
/// provider from subsequent OpenTelemetry collections entirely, exactly how
/// [`record_pool_utilization`] drops a series on `None`. Reporting `0` for an
/// unknown status would be the same false all-clear that `Indicator::Unknown`
/// exists to make unrepresentable.
pub fn record_upstream_status(provider: &str, severity: Option<u8>) {
    match severity {
        Some(severity) => {
            let severity = f64::from(severity);
            sentry::metrics::gauge("shunt.upstream.status", severity)
                .attribute("provider", provider.to_owned())
                .capture();
            upstream_status_values()
                .lock()
                .expect("upstream status metric lock poisoned")
                .insert(provider.to_owned(), severity);
        }
        None => {
            upstream_status_values()
                .lock()
                .expect("upstream status metric lock poisoned")
                .remove(provider);
        }
    }
    let _ = otel_instruments();
}

/// Test-only observation point for [`record_upstream_status`]: the callback-
/// driven `ObservableGauge` only reports on collection by a live OTel reader,
/// so tests that need to prove a provider is (or is not) present in the value
/// map read this instead.
#[cfg(test)]
pub fn upstream_status_value_for_tests(provider: &str) -> Option<f64> {
    upstream_status_values()
        .lock()
        .expect("upstream status metric lock poisoned")
        .get(provider)
        .copied()
}

/// The outcome of a Codex WebSocket continuation decision on a *reused*
/// connection (one that carried stored `previous_response_id` state).
#[derive(Clone, Copy, Debug)]
pub enum ContinuationOutcome {
    /// The input was an append-only extension, so only the delta was sent with
    /// `previous_response_id` — the payload-trimming win.
    Hit,
    /// The input was not an append-only extension of the stored transcript, so
    /// the full input was re-sent. Correctness-safe, but a missed optimization.
    Fallback,
}

impl ContinuationOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Fallback => "fallback",
        }
    }
}

/// Record one proxied inference request: a `shunt.requests` count and a
/// `shunt.latency` distribution, both tagged with provider, model (the
/// client-requested id), and the response status code. Emitted to Sentry and
/// OpenTelemetry; each sink is inert unless configured.
///
/// Client traffic. An internal call a driven `[models.router]` makes goes
/// through [`record_proxied_request_as`] with `caller = "router"`.
pub fn record_proxied_request(provider: &str, model: &str, status: u16, latency_ms: f64) {
    record_proxied_request_as("client", provider, model, status, latency_ms);
}

/// [`record_proxied_request`] with the `caller` attribute spelled out.
///
/// `caller` separates a client turn from an internal judge call (ADR-0005 §3):
/// both ride the same failover chain, the same adapters, and the same account
/// pools, so both belong in the same series — but an operator reading request
/// volume or latency needs to know which half is the gateway's own. A closed
/// two-value set, so it costs one dimension of two.
pub fn record_proxied_request_as(
    caller: &'static str,
    provider: &str,
    model: &str,
    status: u16,
    latency_ms: f64,
) {
    sentry::metrics::counter("shunt.requests", 1)
        .attribute("provider", provider.to_owned())
        .attribute("model", model.to_owned())
        .attribute("caller", caller)
        .attribute("http.response.status_code", i64::from(status))
        .capture();
    sentry::metrics::distribution("shunt.latency", latency_ms)
        .unit(Unit::Millisecond)
        .attribute("provider", provider.to_owned())
        .attribute("model", model.to_owned())
        .attribute("caller", caller)
        .attribute("http.response.status_code", i64::from(status))
        .capture();

    let attributes = [
        KeyValue::new("provider", provider.to_owned()),
        KeyValue::new("model", model.to_owned()),
        KeyValue::new("caller", caller),
        KeyValue::new("http.response.status_code", i64::from(status)),
    ];
    let instruments = otel_instruments();
    instruments.requests.add(1, &attributes);
    instruments.latency.record(latency_ms, &attributes);

    #[cfg(test)]
    {
        let mut samples = test_proxied_samples()
            .lock()
            .expect("test proxied-request sample lock poisoned");
        let entry = samples
            .entry((caller, provider.to_owned(), model.to_owned(), status))
            .or_default();
        entry.count += 1;
        entry.latencies.push(latency_ms);
    }
}

/// One test-observed [`record_proxied_request`] sample: the count and every
/// latency for one (provider, model, status) key.
#[cfg(test)]
#[derive(Default)]
struct ProxiedRequestSample {
    count: u64,
    latencies: Vec<f64>,
}

#[cfg(test)]
type ProxiedSampleStore = Mutex<HashMap<(&'static str, String, String, u16), ProxiedRequestSample>>;

#[cfg(test)]
fn test_proxied_samples() -> &'static ProxiedSampleStore {
    static SAMPLES: OnceLock<ProxiedSampleStore> = OnceLock::new();
    SAMPLES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Test-only observation point for [`record_proxied_request`]: both metric
/// sinks are inert unless an endpoint is configured, so tests that must
/// prove a sample was recorded — or skipped — read the per-key count and
/// latencies from the test store instead.
#[cfg(test)]
pub fn proxied_request_samples_for_tests(
    provider: &str,
    model: &str,
    status: u16,
) -> (u64, Vec<f64>) {
    // Summed across callers, deliberately: this is the whole-series view its
    // existing callers have always read, and a judge call is a proxied request
    // like any other. Use `proxied_request_samples_by_caller_for_tests` to
    // separate the two halves.
    test_proxied_samples()
        .lock()
        .expect("test proxied-request sample lock poisoned")
        .iter()
        .filter(|((_, sample_provider, sample_model, sample_status), _)| {
            sample_provider == provider && sample_model == model && *sample_status == status
        })
        .fold((0, Vec::new()), |(count, mut latencies), (_, sample)| {
            latencies.extend(sample.latencies.iter().copied());
            (count + sample.count, latencies)
        })
}

/// [`proxied_request_samples_for_tests`] narrowed to one `caller`, for the
/// tests that must prove a judge call is attributed to the router and a client
/// turn is not.
#[cfg(test)]
pub fn proxied_request_samples_by_caller_for_tests(
    caller: &'static str,
    provider: &str,
    model: &str,
    status: u16,
) -> (u64, Vec<f64>) {
    test_proxied_samples()
        .lock()
        .expect("test proxied-request sample lock poisoned")
        .get(&(caller, provider.to_owned(), model.to_owned(), status))
        .map_or((0, Vec::new()), |sample| {
            (sample.count, sample.latencies.clone())
        })
}

/// Record a Codex WebSocket continuation decision on a reused connection: a
/// `hit` (continued from `previous_response_id`, delta only) or a `fallback`
/// (input was not an append-only extension, full input re-sent). Emitted only
/// when the pooled connection actually held continuation state, so the two
/// series are directly comparable — a fresh connection (no stored state) is not
/// counted. A rising `fallback` share on a warm pool is the signal that the
/// append-only normalization has drifted from the backend's item shapes (issue
/// #45): correctness-safe, but a latent lost optimization. Emitted to Sentry and
/// OpenTelemetry; each sink is inert unless configured.
pub fn record_continuation_outcome(provider: &str, outcome: ContinuationOutcome) {
    let provider = provider.to_owned();
    let outcome = outcome.as_str();
    sentry::metrics::counter("shunt.codex_continuation", 1)
        .attribute("provider", provider.clone())
        .attribute("outcome", outcome.to_owned())
        .capture();

    let attributes = [
        KeyValue::new("provider", provider),
        KeyValue::new("outcome", outcome),
    ];
    otel_instruments().continuation.add(1, &attributes);
}

/// Record one sanitized Codex CLI product-analytics event name. The caller
/// (`codex_analytics`) guarantees the `event` attribute is sanitized to a
/// bounded character set and length and capped to a finite number of distinct
/// names; no event properties or payload data reach either sink.
pub fn record_codex_client_event(event: &str) {
    sentry::metrics::counter("shunt.codex_client_events", 1)
        .attribute("event", event.to_owned())
        .capture();

    let attributes = [KeyValue::new("event", event.to_owned())];
    otel_instruments().codex_client_events.add(1, &attributes);
}

/// Record one inbound gateway OTLP payload (issue #189), tagged with the
/// `signal` it was posted for (`metrics`/`logs`/`traces`) and the `outcome`:
/// `relayed` when at least one destination opted in, `discarded` when none did,
/// `shed` when the in-flight relay limit was saturated so nothing could be
/// relayed, and `rejected` for a request refused before ingest (bad bearer,
/// unreadable or over-cap body). Both attributes are fixed strings chosen by
/// shunt — no part of the payload, its headers, or the destination reaches
/// either sink.
pub fn record_gateway_telemetry_ingest(signal: &'static str, outcome: &'static str) {
    sentry::metrics::counter("shunt.gateway_telemetry_ingest", 1)
        .attribute("signal", signal.to_owned())
        .attribute("outcome", outcome.to_owned())
        .capture();

    let attributes = [
        KeyValue::new("signal", signal),
        KeyValue::new("outcome", outcome),
    ];
    otel_instruments()
        .gateway_telemetry_ingest
        .add(1, &attributes);
}

/// Record one bounded upstream retry (issue #48): a `shunt.upstream_retries`
/// count tagged with the provider and a low-cardinality `reason` — the transient
/// status (`429`/`502`/`503`/`504`) or `transport` for a connection-level error.
/// A rising count signals a flaky upstream that retries are papering over.
/// Emitted to Sentry and OpenTelemetry; each sink is inert unless configured.
pub fn record_upstream_retry(provider: &str, reason: &'static str) {
    sentry::metrics::counter("shunt.upstream_retries", 1)
        .attribute("provider", provider.to_owned())
        .attribute("reason", reason.to_owned())
        .capture();

    let attributes = [
        KeyValue::new("provider", provider.to_owned()),
        KeyValue::new("reason", reason),
    ];
    otel_instruments().upstream_retries.add(1, &attributes);
}

/// Record one ordered-upstream failover transition. `state` is one of
/// `attempted`, `advanced`, or `exhausted`.
pub fn record_failover(provider: &str, state: &'static str) {
    sentry::metrics::counter("shunt.failover", 1)
        .attribute("provider", provider.to_owned())
        .attribute("state", state.to_owned())
        .capture();

    let attributes = [
        KeyValue::new("provider", provider.to_owned()),
        KeyValue::new("state", state),
    ];
    otel_instruments().failover.add(1, &attributes);
}

/// Record one stage-router tier decision (issue #543 follow-up; plan PR 6).
///
/// `model` is the `[[models]]` entry carrying the router, not the tier target,
/// so the series stays one per configured router rather than one per target.
/// It must be the id the router was *matched* on rather than the raw request
/// id: a client-side `[1m]` context-window hint is stripped before the lookup
/// and before the session is keyed, so labelling by the raw id would report one
/// router as two series and one session's pin under both. `StageOutcome::model`
/// carries that id out of routing for exactly this reason.
///
/// `tier` and `source` are closed sets (`StageTier::as_label`,
/// `StageSource::as_label`), so the label space is the number of routers times
/// ten, whatever the session count.
///
/// Called only for an admitted request. A turn rejected by inbound auth or the
/// managed-model policy is routed but never served, and counting it would
/// report traffic the gateway did not carry.
pub fn record_stage_decision(model: &str, tier: &'static str, source: &'static str) {
    sentry::metrics::counter("shunt.stage_router.decisions", 1)
        .attribute("model", model.to_owned())
        .attribute("tier", tier)
        .attribute("source", source)
        .capture();

    let attributes = [
        KeyValue::new("model", model.to_owned()),
        KeyValue::new("tier", tier),
        KeyValue::new("source", source),
    ];
    otel_instruments().stage_decisions.add(1, &attributes);
}

/// Record one `[models.router]` decision, whatever the algorithm (ADR-0005 §7).
///
/// The series a reader totals *across* router types, which
/// `shunt.stage_router.decisions` cannot be: that counter reports a tier, and
/// `random` and `noop` have none. Both are recorded for a stage turn — the
/// shipped one keeps its exact shape, and this one adds the algorithm and the
/// chosen target beside it.
///
/// `model` is the `[[models]]` entry carrying the router, matched past
/// `strip_context_window_hint` for the same reason
/// [`record_stage_decision`] documents. `algorithm` and `source` are closed
/// sets (`RouterConfig::algorithm`, `SubagentsConfig::algorithm`,
/// `RouteSource::as_label`). `target` is a
/// configured model id, so its cardinality is the operator's target list, not
/// the client's traffic.
///
/// Called only for an admitted request, and never for a `count_tokens` probe —
/// the same admission boundary [`record_stage_decision`] observes.
pub fn record_router_decision(
    model: &str,
    algorithm: &'static str,
    target: &str,
    source: &'static str,
) {
    sentry::metrics::counter("shunt.router.decisions", 1)
        .attribute("model", model.to_owned())
        .attribute("algorithm", algorithm)
        .attribute("target", target.to_owned())
        .attribute("source", source)
        .capture();

    let attributes = [
        KeyValue::new("model", model.to_owned()),
        KeyValue::new("algorithm", algorithm),
        KeyValue::new("target", target.to_owned()),
        KeyValue::new("source", source),
    ];
    otel_instruments().router_decisions.add(1, &attributes);
}

/// Record one judge consultation a driven `[models.router]` turn earned
/// (ADR-0005 §7).
///
/// Counted per *turn that wanted a judge*, not per upstream call: a turn whose
/// session had already spent `max_judge_calls` is recorded here with
/// `outcome = "budget_exhausted"` without any call being made, which is what
/// makes this series the one an operator tunes that key against. The upstream
/// call itself, when there is one, appears in `shunt.requests` with
/// `caller = "router"`.
///
/// `model` is the `[[models]]` entry carrying the router, matched past
/// `strip_context_window_hint` for the same reason [`record_router_decision`]
/// documents. `algorithm` is a closed set (`RouterConfig::algorithm`), and so
/// is `outcome` — `decided` plus the fail-open labels on
/// `crate::routing::judge::JudgeOutcome`. Nothing from the verdict itself
/// reaches this series.
pub fn record_judge_call(model: &str, algorithm: &'static str, outcome: &'static str) {
    sentry::metrics::counter("shunt.router.judge_calls", 1)
        .attribute("model", model.to_owned())
        .attribute("algorithm", algorithm)
        .attribute("outcome", outcome)
        .capture();

    let attributes = [
        KeyValue::new("model", model.to_owned()),
        KeyValue::new("algorithm", algorithm),
        KeyValue::new("outcome", outcome),
    ];
    otel_instruments().judge_calls.add(1, &attributes);

    #[cfg(test)]
    {
        *test_judge_call_samples()
            .lock()
            .expect("test judge-call sample lock poisoned")
            .entry((model.to_owned(), algorithm, outcome))
            .or_default() += 1;
    }
}

#[cfg(test)]
type JudgeCallSampleStore = Mutex<HashMap<(String, &'static str, &'static str), u64>>;

#[cfg(test)]
fn test_judge_call_samples() -> &'static JudgeCallSampleStore {
    static SAMPLES: OnceLock<JudgeCallSampleStore> = OnceLock::new();
    SAMPLES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Test-only observation point for [`record_judge_call`]: both metric sinks are
/// inert unless an endpoint is configured, so a test that must prove a
/// consultation was counted — or skipped — reads the per-key count here.
#[cfg(test)]
pub fn judge_call_samples_for_tests(
    model: &str,
    algorithm: &'static str,
    outcome: &'static str,
) -> u64 {
    test_judge_call_samples()
        .lock()
        .expect("test judge-call sample lock poisoned")
        .get(&(model.to_owned(), algorithm, outcome))
        .copied()
        .unwrap_or(0)
}

/// Record one stage-router decision that moved a session off its pinned tier.
///
/// A flip is the expensive event this design is built to ration. It always
/// forfeits a warmed prompt-cache prefix, since caching is keyed per model; on
/// the Codex transport it also forces a full-input re-send, because `model` is
/// hashed into the continuation signature; and a flip that crosses providers
/// abandons the session's pooled socket and sticky account slot too. The decision
/// counter above cannot show it — a session pinned to `capable` and a session
/// that just moved there are the same row — so churn needs its own series.
///
/// `from` and `to` are tier labels, which makes the two directions separable:
/// escalation is designed to be easy and de-escalation hard, so they are not
/// expected to be symmetric and a single count would hide that.
///
/// The pair comes from what `StageRouterStore::commit` actually wrote, not from
/// the tier the deciding request saw. Those differ under concurrency: the store
/// lock is released between deciding and committing, so two turns of one
/// session can both read `efficient` and both choose `capable`, and only one of
/// them displaces anything.
pub fn record_stage_flip(model: &str, from: &'static str, to: &'static str) {
    sentry::metrics::counter("shunt.stage_router.flips", 1)
        .attribute("model", model.to_owned())
        .attribute("from", from)
        .attribute("to", to)
        .capture();

    let attributes = [
        KeyValue::new("model", model.to_owned()),
        KeyValue::new("from", from),
        KeyValue::new("to", to),
    ];
    otel_instruments().stage_flips.add(1, &attributes);
}

/// Record one inbound request shed at the `[server] max_concurrent_requests`
/// limit (issue #260). A shed request never reaches a handler, so it is absent
/// from [`record_proxied_request`] and from the per-request spans — without this
/// counter a saturated gateway is invisible at the default `shunt=info` filter,
/// since the rejection itself is logged only at `debug!`.
pub fn record_request_shed() {
    sentry::metrics::counter("shunt.requests_shed", 1).capture();
    otel_instruments().requests_shed.add(1, &[]);
}

/// The outcome of a Codex WebSocket dedicated-overflow admission decision
/// (issue #248): a concurrent turn found the session's pooled connection
/// already streaming and either opened a one-shot dedicated socket, or was
/// refused because the overflow ceiling (`MAX_OVERFLOW_CONNECTIONS` in
/// `crate::adapters::responses::codex_ws`) was already saturated.
#[derive(Clone, Copy, Debug)]
pub enum CodexWsOverflowOutcome {
    /// The pooled connection was busy; a dedicated overflow socket was opened
    /// for this turn. It carries no `previous_response_id` continuation, so a
    /// rising count on a session that expects one shared connection answers
    /// issue #248's open question: concurrent Claude Code agents are sharing
    /// one `x-claude-code-session-id` and colliding on the same pooled turn.
    Opened,
    /// The overflow ceiling was already saturated; admission was refused
    /// before any frame was sent and the caller falls back to HTTP.
    Refused,
}

impl CodexWsOverflowOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Opened => "opened",
            Self::Refused => "refused",
        }
    }
}

/// Record one Codex WebSocket dedicated-overflow admission decision: a
/// `shunt.codex_ws_overflow` count tagged with the provider and `outcome`
/// (`opened` or `refused`). Emitted to Sentry and OpenTelemetry; each sink is
/// inert unless configured. See [`CodexWsOverflowOutcome`].
pub fn record_codex_ws_overflow(provider: &str, outcome: CodexWsOverflowOutcome) {
    let provider = provider.to_owned();
    let outcome_str = outcome.as_str();
    sentry::metrics::counter("shunt.codex_ws_overflow", 1)
        .attribute("provider", provider.clone())
        .attribute("outcome", outcome_str)
        .capture();

    #[cfg(test)]
    {
        *test_overflow_counts()
            .lock()
            .expect("test overflow counter lock poisoned")
            .entry((provider.clone(), outcome_str))
            .or_insert(0) += 1;
    }

    let attributes = [
        KeyValue::new("provider", provider),
        KeyValue::new("outcome", outcome_str),
    ];
    otel_instruments().codex_ws_overflow.add(1, &attributes);
}

/// Test-only observation point for [`record_codex_ws_overflow`]: neither sink
/// (Sentry, OpenTelemetry) is introspectable without a live collector, so
/// callers that need to prove the counter actually increments (rather than
/// merely not panicking) read this instead. Keyed by provider so tests using
/// distinct provider strings do not interfere with each other's counts even
/// when `cargo test` runs them in parallel within one process.
#[cfg(test)]
pub fn codex_ws_overflow_count_for_tests(provider: &str, outcome: CodexWsOverflowOutcome) -> u64 {
    *test_overflow_counts()
        .lock()
        .expect("test overflow counter lock poisoned")
        .get(&(provider.to_string(), outcome.as_str()))
        .unwrap_or(&0)
}

#[cfg(test)]
fn test_overflow_counts() -> &'static Mutex<HashMap<(String, &'static str), u64>> {
    static COUNTS: OnceLock<Mutex<HashMap<(String, &'static str), u64>>> = OnceLock::new();
    COUNTS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::{
        judge_call_samples_for_tests, proxied_request_samples_by_caller_for_tests,
        record_codex_client_event, record_continuation_outcome, record_gateway_telemetry_ingest,
        record_judge_call, record_pool_rotation, record_pool_utilization, record_proxied_request,
        record_proxied_request_as, record_stream_outcome, record_stream_tokens, record_ttft,
        record_upstream_status, upstream_status_value_for_tests, ContinuationOutcome,
    };

    /// The core opt-in contract: recording a proxied request must never panic,
    /// whatever the sink state — the default (no Sentry client, no OTel meter
    /// provider) and any ambient global provider a sibling test may have
    /// installed (globals are process-wide, so this test can't assume none is
    /// bound). Emission stays a silent no-op when nothing is configured.
    #[test]
    fn record_is_noop_without_sinks() {
        record_proxied_request("openai", "gpt-5.2", 200, 123.4);
        record_proxied_request("anthropic", "claude-opus-4-8", 502, 0.0);
    }

    /// A consultation is counted per turn that wanted a judge, keyed by model,
    /// algorithm and outcome. Two outcomes of one model must not collapse into
    /// one series — the fail-open labels are the whole point of the counter, and
    /// an operator reads `budget_exhausted` against `max_judge_calls` and
    /// `timeout` against `judge_timeout_ms`.
    #[test]
    fn a_judge_consultation_is_counted_per_outcome() {
        // A model id of this test's own, because the sample store is
        // process-wide and shared with every other test in the binary.
        let model = "claude-auto-judge-metric";
        record_judge_call(model, "stage_router", "decided");
        record_judge_call(model, "stage_router", "decided");
        record_judge_call(model, "stage_router", "timeout");

        assert_eq!(
            judge_call_samples_for_tests(model, "stage_router", "decided"),
            2
        );
        assert_eq!(
            judge_call_samples_for_tests(model, "stage_router", "timeout"),
            1
        );
        assert_eq!(
            judge_call_samples_for_tests(model, "stage_router", "budget_exhausted"),
            0,
            "an outcome nothing recorded must not borrow another's count"
        );
    }

    /// The judge's upstream call lands in `shunt.requests` like any other, and
    /// the `caller` attribute is what separates it from the client turn it was
    /// made on behalf of. Drop the attribute and an operator reading per-model
    /// request volume cannot tell the two apart.
    #[test]
    fn the_caller_attribute_separates_a_judge_call_from_a_client_turn() {
        let model = "claude-judge-caller-metric";
        record_proxied_request("judge", model, 200, 1.0);
        record_proxied_request_as("router", "judge", model, 200, 2.0);

        let (client, _) =
            proxied_request_samples_by_caller_for_tests("client", "judge", model, 200);
        let (router, _) =
            proxied_request_samples_by_caller_for_tests("router", "judge", model, 200);
        assert_eq!(client, 1, "the plain recorder is the client caller");
        assert_eq!(router, 1, "the judge call is attributed to the router");
    }

    /// Stream metrics honor the same opt-in no-op contract.
    #[test]
    fn record_stream_metrics_are_noop_without_sinks() {
        record_ttft("anthropic", "claude-opus-4-8", 42.0);
        record_stream_outcome("anthropic", "claude-opus-4-8", "completed");
        record_stream_tokens("anthropic", "claude-opus-4-8", "input", 123);
    }

    /// Pool metrics honor the same opt-in no-op contract.
    #[test]
    fn record_pool_metrics_are_noop_without_sinks() {
        record_pool_utilization("anthropic", "5h", Some(0.25));
        record_pool_utilization("anthropic", "5h", None);
        record_pool_rotation("anthropic", "rate_limit");
    }

    /// A known-good severity is observable in the value map; an `Unknown`
    /// provider (`severity: None`) must be *absent* from it, not present with
    /// a `0` value — the metric-side half of `Indicator::Unknown` never
    /// collapsing to `Indicator::None`.
    #[test]
    fn unknown_upstream_status_produces_no_gauge_sample() {
        let provider = "status-metrics-test-provider";
        record_upstream_status(provider, Some(2));
        assert_eq!(upstream_status_value_for_tests(provider), Some(2.0));

        record_upstream_status(provider, None);
        assert_eq!(upstream_status_value_for_tests(provider), None);
    }

    /// The continuation counter honors the same opt-in no-op contract.
    #[test]
    fn record_continuation_is_noop_without_sinks() {
        record_continuation_outcome("codex", ContinuationOutcome::Hit);
        record_continuation_outcome("codex", ContinuationOutcome::Fallback);
    }

    /// The Codex client-event counter honors the same opt-in no-op contract.
    #[test]
    fn record_codex_client_event_is_noop_without_sinks() {
        record_codex_client_event("codex.turn_completed");
    }

    /// The gateway telemetry-ingest counter honors the same opt-in no-op contract.
    #[test]
    fn record_gateway_telemetry_ingest_is_noop_without_sinks() {
        record_gateway_telemetry_ingest("metrics", "relayed");
        record_gateway_telemetry_ingest("logs", "discarded");
        record_gateway_telemetry_ingest("traces", "rejected");
    }

    /// The upstream-retry counter honors the same opt-in no-op contract.
    #[test]
    fn record_upstream_retry_is_noop_without_sinks() {
        super::record_upstream_retry("anthropic", "503");
        super::record_upstream_retry("openai", "transport");
    }

    /// The failover counter honors the same opt-in no-op contract.
    #[test]
    fn record_failover_is_noop_without_sinks() {
        super::record_failover("anthropic", "attempted");
        super::record_failover("openai", "advanced");
        super::record_failover("openai", "exhausted");
    }

    /// The stage-router counters honor the same opt-in no-op contract.
    #[test]
    fn record_stage_counters_are_noop_without_sinks() {
        super::record_router_decision("claude-auto", "random", "claude-sonnet-4-6", "random");
        super::record_stage_decision("claude-auto", "capable", "override");
        super::record_stage_decision("claude-auto", "efficient", "no_signal");
        super::record_stage_flip("claude-auto", "efficient", "capable");
    }

    /// The Codex WebSocket overflow counter honors the same opt-in no-op
    /// contract, and — unlike the other counters here — the increment is also
    /// independently observable through the test-only accessor, proving the
    /// counter actually counts rather than merely not panicking.
    #[test]
    fn record_codex_ws_overflow_is_noop_without_sinks_and_counts_for_tests() {
        use super::{
            codex_ws_overflow_count_for_tests, record_codex_ws_overflow, CodexWsOverflowOutcome,
        };

        let provider = "codex-metrics-unit-test";
        assert_eq!(
            codex_ws_overflow_count_for_tests(provider, CodexWsOverflowOutcome::Opened),
            0
        );
        assert_eq!(
            codex_ws_overflow_count_for_tests(provider, CodexWsOverflowOutcome::Refused),
            0
        );

        record_codex_ws_overflow(provider, CodexWsOverflowOutcome::Opened);
        record_codex_ws_overflow(provider, CodexWsOverflowOutcome::Opened);
        record_codex_ws_overflow(provider, CodexWsOverflowOutcome::Refused);

        assert_eq!(
            codex_ws_overflow_count_for_tests(provider, CodexWsOverflowOutcome::Opened),
            2
        );
        assert_eq!(
            codex_ws_overflow_count_for_tests(provider, CodexWsOverflowOutcome::Refused),
            1
        );
    }
}
