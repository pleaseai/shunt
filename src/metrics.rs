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
//! and streaming token usage, Codex continuation decisions and sanitized client
//! analytics event names, and retries. Pool metrics expose best-account quota
//! utilization and account rotations.
//!
//! Attributes stay low-cardinality (provider/model/status/outcome/kind/window/
//! reason, plus the sanitized, cardinality-capped `event` on
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
    upstream_retries: Counter<u64>,
    _pool_utilization: ObservableGauge<f64>,
    pool_rotations: Counter<u64>,
}

type PoolUtilizationValues = HashMap<(String, &'static str), Option<f64>>;

fn pool_utilization_values() -> &'static Mutex<PoolUtilizationValues> {
    static VALUES: OnceLock<Mutex<PoolUtilizationValues>> = OnceLock::new();
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
            upstream_retries: meter
                .u64_counter("shunt.upstream_retries")
                .with_description(
                    "Bounded upstream retries issued for transient failures (issue #48)",
                )
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
pub fn record_proxied_request(provider: &str, model: &str, status: u16, latency_ms: f64) {
    sentry::metrics::counter("shunt.requests", 1)
        .attribute("provider", provider.to_owned())
        .attribute("model", model.to_owned())
        .attribute("http.response.status_code", i64::from(status))
        .capture();
    sentry::metrics::distribution("shunt.latency", latency_ms)
        .unit(Unit::Millisecond)
        .attribute("provider", provider.to_owned())
        .attribute("model", model.to_owned())
        .attribute("http.response.status_code", i64::from(status))
        .capture();

    let attributes = [
        KeyValue::new("provider", provider.to_owned()),
        KeyValue::new("model", model.to_owned()),
        KeyValue::new("http.response.status_code", i64::from(status)),
    ];
    let instruments = otel_instruments();
    instruments.requests.add(1, &attributes);
    instruments.latency.record(latency_ms, &attributes);
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

#[cfg(test)]
mod tests {
    use super::{
        record_codex_client_event, record_continuation_outcome, record_pool_rotation,
        record_pool_utilization, record_proxied_request, record_stream_outcome,
        record_stream_tokens, record_ttft, ContinuationOutcome,
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

    /// The upstream-retry counter honors the same opt-in no-op contract.
    #[test]
    fn record_upstream_retry_is_noop_without_sinks() {
        super::record_upstream_retry("anthropic", "503");
        super::record_upstream_retry("openai", "transport");
    }
}
