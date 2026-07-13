//! Usage/performance metric emission.
//!
//! Every proxied inference request is recorded to two independent, opt-in
//! sinks — each a no-op unless its section is configured:
//!
//! - **Sentry** (`[sentry] metrics = true`): a `shunt.requests` count and a
//!   `shunt.latency` distribution. Dropped by the SDK when no client is bound
//!   or `enable_metrics` is off.
//! - **OpenTelemetry** (`[otel]` with `metrics = true`): the same two series on
//!   the global meter. A no-op until `crate::telemetry::init` installs a meter
//!   provider, so with `[otel]` absent the instruments are inert.
//!
//! Attributes stay low-cardinality (provider/model/status) — never client
//! names, session ids, or anything request-derived.

use std::sync::OnceLock;

use opentelemetry::{
    metrics::{Counter, Histogram},
    KeyValue,
};
use sentry::protocol::Unit;

/// OTel instruments on the global meter. Created lazily on first record so the
/// meter provider (installed at startup, before any request) is already in
/// place; with `[otel]` disabled the global meter is a no-op and so are these.
struct OtelInstruments {
    requests: Counter<u64>,
    latency: Histogram<f64>,
    continuation: Counter<u64>,
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
            continuation: meter
                .u64_counter("shunt.codex_continuation")
                .with_description(
                    "Codex WebSocket continuation decisions (hit vs full-input fallback)",
                )
                .build(),
        }
    })
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

#[cfg(test)]
mod tests {
    use super::{record_continuation_outcome, record_proxied_request, ContinuationOutcome};

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

    /// The continuation counter honors the same opt-in no-op contract.
    #[test]
    fn record_continuation_is_noop_without_sinks() {
        record_continuation_outcome("codex", ContinuationOutcome::Hit);
        record_continuation_outcome("codex", ContinuationOutcome::Fallback);
    }
}
